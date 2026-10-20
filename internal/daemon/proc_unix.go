//go:build !windows

package daemon

import (
	"os"
	"os/exec"
	"syscall"
)

// detach puts the process in a new session so terminal signals and closure
// never reach it.
func detach(cmd *exec.Cmd) {
	cmd.SysProcAttr = &syscall.SysProcAttr{Setsid: true}
}

// Alive reports whether pid refers to a running process.
func Alive(pid int) bool {
	if pid <= 0 {
		return false
	}
	process, err := os.FindProcess(pid)
	if err != nil {
		return false
	}
	defer process.Release()
	return process.Signal(syscall.Signal(0)) == nil
}

// terminate asks the gateway to shut down gracefully.
func terminate(pid int) error {
	process, err := os.FindProcess(pid)
	if err != nil {
		return err
	}
	defer process.Release()
	return process.Signal(syscall.SIGTERM)
}

// kill ends the process immediately.
func kill(pid int) error {
	process, err := os.FindProcess(pid)
	if err != nil {
		return err
	}
	defer process.Release()
	return process.Signal(syscall.SIGKILL)
}
