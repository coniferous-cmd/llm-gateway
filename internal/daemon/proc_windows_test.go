//go:build windows

package daemon

import (
	"os"
	"os/exec"
	"syscall"
	"testing"
)

// TestAliveExitedProcessWithHeldHandle pins the liveness check to a process
// that has exited but is still reachable through a held handle, the state CI
// runners leave behind right after a child terminates. Opening the process
// succeeds, yet it must count as dead so stale pid files are still cleaned.
func TestAliveExitedProcessWithHeldHandle(t *testing.T) {
	cmd := exec.Command(os.Args[0], "-test.run=TestNoMatchSoWeExitImmediately")
	if err := cmd.Start(); err != nil {
		t.Fatalf("start helper process: %v", err)
	}
	pid := cmd.Process.Pid
	// Our own handle keeps the process object alive after the process exits.
	handle, err := syscall.OpenProcess(syscall.SYNCHRONIZE, false, uint32(pid))
	if err != nil {
		cmd.Process.Kill()
		t.Fatalf("open helper process: %v", err)
	}
	defer syscall.CloseHandle(handle)
	if err := cmd.Wait(); err != nil {
		t.Fatalf("wait helper process: %v", err)
	}
	if Alive(pid) {
		t.Fatalf("exited pid %d reported alive while only its handle is held", pid)
	}
}
