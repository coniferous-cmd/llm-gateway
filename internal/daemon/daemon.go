// Package daemon detaches the gateway from the terminal and stops it via a pid file.
package daemon

import (
	"errors"
	"fmt"
	"os"
	"os/exec"
	"strconv"
	"strings"
	"time"
)

// ErrNotRunning reports that the pid file holds no live gateway process.
var ErrNotRunning = errors.New("gateway is not running")

// Start launches exe detached from the current terminal, with stdout and stderr
// appended to logPath. The caller does not wait for the child; it keeps running
// after this process exits.
func Start(exe string, args []string, logPath string) (*exec.Cmd, error) {
	log, err := os.OpenFile(logPath, os.O_CREATE|os.O_WRONLY|os.O_APPEND, 0600)
	if err != nil {
		return nil, fmt.Errorf("open log file %s: %w", logPath, err)
	}
	defer log.Close()
	cmd := exec.Command(exe, args...)
	cmd.Stdin, cmd.Stdout, cmd.Stderr = nil, log, log
	detach(cmd)
	if err := cmd.Start(); err != nil {
		return nil, fmt.Errorf("start background process: %w", err)
	}
	return cmd, nil
}

// WritePID records pid in path so Stop can find the running gateway.
func WritePID(path string, pid int) error {
	if err := os.WriteFile(path, []byte(strconv.Itoa(pid)+"\n"), 0600); err != nil {
		return fmt.Errorf("write pid file %s: %w", path, err)
	}
	return nil
}

// ReadPID returns the process id recorded in path.
func ReadPID(path string) (int, error) {
	data, err := os.ReadFile(path)
	if err != nil {
		return 0, err
	}
	text := strings.TrimSpace(string(data))
	pid, err := strconv.Atoi(text)
	if err != nil || pid <= 0 {
		return 0, fmt.Errorf("pid file %s: invalid contents %q", path, text)
	}
	return pid, nil
}

// RemovePID deletes path only while it still records pid, so one instance can
// never delete another instance's pid file.
func RemovePID(path string, pid int) {
	if current, err := ReadPID(path); err != nil || current != pid {
		return
	}
	_ = os.Remove(path)
}

// RunningPID reports the id of the live gateway recorded in the pid file:
// (0, nil) when none is running and (0, err) when the file cannot be read.
// A stale entry pointing at a dead process is removed.
func RunningPID(path string) (int, error) {
	pid, err := ReadPID(path)
	if err != nil {
		if os.IsNotExist(err) {
			return 0, nil
		}
		return 0, err
	}
	if Alive(pid) {
		return pid, nil
	}
	RemovePID(path, pid)
	return 0, nil
}

// Stop terminates the gateway recorded in the pid file and removes the file.
// On POSIX the process receives SIGTERM and gets up to grace to drain before it
// is killed; Windows has no cross-process graceful signal, so it is terminated
// immediately.
func Stop(pidPath string, grace time.Duration) (int, error) {
	pid, err := ReadPID(pidPath)
	if err != nil {
		if os.IsNotExist(err) {
			return 0, fmt.Errorf("%w (no pid file at %s)", ErrNotRunning, pidPath)
		}
		return 0, err
	}
	if !Alive(pid) {
		RemovePID(pidPath, pid)
		return 0, fmt.Errorf("%w (stale pid file for %d removed)", ErrNotRunning, pid)
	}
	if err := terminate(pid); err != nil {
		return 0, fmt.Errorf("stop pid %d: %w", pid, err)
	}
	if !waitExit(pid, grace) {
		if err := kill(pid); err != nil {
			return 0, fmt.Errorf("kill pid %d: %w", pid, err)
		}
		if !waitExit(pid, 2*time.Second) {
			return 0, fmt.Errorf("pid %d did not exit after kill", pid)
		}
	}
	RemovePID(pidPath, pid)
	return pid, nil
}

func waitExit(pid int, timeout time.Duration) bool {
	deadline := time.Now().Add(timeout)
	for Alive(pid) {
		if time.Now().After(deadline) {
			return false
		}
		time.Sleep(100 * time.Millisecond)
	}
	return true
}
