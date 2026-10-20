package daemon

import (
	"os"
	"os/exec"
	"path/filepath"
	"testing"
	"time"
)

func TestPIDFileRoundtrip(t *testing.T) {
	path := filepath.Join(t.TempDir(), "gateway.pid")
	if err := WritePID(path, 4321); err != nil {
		t.Fatal(err)
	}
	pid, err := ReadPID(path)
	if err != nil || pid != 4321 {
		t.Fatalf("ReadPID = %d, %v; want 4321", pid, err)
	}
	RemovePID(path, 9999)
	if _, err := os.Stat(path); err != nil {
		t.Fatalf("pid file removed by a foreign pid: %v", err)
	}
	RemovePID(path, 4321)
	if _, err := os.Stat(path); !os.IsNotExist(err) {
		t.Fatalf("pid file still present: %v", err)
	}
}

func TestReadPIDInvalid(t *testing.T) {
	path := filepath.Join(t.TempDir(), "gateway.pid")
	if err := os.WriteFile(path, []byte("not-a-pid\n"), 0600); err != nil {
		t.Fatal(err)
	}
	if _, err := ReadPID(path); err == nil {
		t.Fatal("want error for invalid pid file contents")
	}
}

func TestRunningPID(t *testing.T) {
	path := filepath.Join(t.TempDir(), "gateway.pid")
	if pid, err := RunningPID(path); pid != 0 || err != nil {
		t.Fatalf("RunningPID without file = %d, %v; want 0, nil", pid, err)
	}
	if err := WritePID(path, os.Getpid()); err != nil {
		t.Fatal(err)
	}
	if pid, err := RunningPID(path); pid != os.Getpid() || err != nil {
		t.Fatalf("RunningPID = %d, %v; want %d, nil", pid, err, os.Getpid())
	}
	if err := WritePID(path, deadPID(t)); err != nil {
		t.Fatal(err)
	}
	if pid, err := RunningPID(path); pid != 0 || err != nil {
		t.Fatalf("RunningPID with stale entry = %d, %v; want 0, nil", pid, err)
	}
	if _, err := os.Stat(path); !os.IsNotExist(err) {
		t.Fatalf("stale pid file not cleaned: %v", err)
	}
}

func TestStopWithoutPIDFile(t *testing.T) {
	path := filepath.Join(t.TempDir(), "gateway.pid")
	if pid, err := Stop(path, time.Second); err == nil || pid != 0 {
		t.Fatalf("Stop without pid file = %d, %v; want 0, error", pid, err)
	}
}

func TestStopStalePIDFile(t *testing.T) {
	path := filepath.Join(t.TempDir(), "gateway.pid")
	if err := WritePID(path, deadPID(t)); err != nil {
		t.Fatal(err)
	}
	if pid, err := Stop(path, time.Second); err == nil || pid != 0 {
		t.Fatalf("Stop with stale pid file = %d, %v; want 0, error", pid, err)
	}
	if _, err := os.Stat(path); !os.IsNotExist(err) {
		t.Fatalf("stale pid file not cleaned: %v", err)
	}
}

// deadPID runs this test binary to completion and returns its now-unused pid.
func deadPID(t *testing.T) int {
	t.Helper()
	cmd := exec.Command(os.Args[0], "-test.run=TestNoMatchSoWeExitImmediately")
	if err := cmd.Run(); err != nil {
		t.Fatalf("run helper process: %v", err)
	}
	return cmd.Process.Pid
}
