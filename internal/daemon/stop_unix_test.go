//go:build !windows

package daemon

import (
	"os"
	"os/exec"
	"os/signal"
	"path/filepath"
	"syscall"
	"testing"
	"time"
)

// Stop must escalate to SIGKILL when the recorded process ignores SIGTERM and
// outlives the grace window.
func TestStopKillsSIGTERMResistantProcess(t *testing.T) {
	dir := t.TempDir()
	ready := filepath.Join(dir, "ready")
	cmd := exec.Command(os.Args[0], "-test.run=TestHelperIgnoreSIGTERM")
	cmd.Env = append(os.Environ(), "LLM_GATEWAY_TEST_HELPER=sigterm-resistant", "LLM_GATEWAY_TEST_READY="+ready)
	if err := cmd.Start(); err != nil {
		t.Fatal(err)
	}
	defer func() {
		_ = cmd.Process.Kill()
		_ = cmd.Wait()
	}()
	awaitFile(t, ready)
	path := filepath.Join(dir, "gateway.pid")
	if err := WritePID(path, cmd.Process.Pid); err != nil {
		t.Fatal(err)
	}
	pid, err := Stop(path, 500*time.Millisecond)
	if err != nil || pid != cmd.Process.Pid {
		t.Fatalf("Stop = %d, %v; want %d, nil", pid, err, cmd.Process.Pid)
	}
	if Alive(cmd.Process.Pid) {
		t.Fatal("SIGTERM-resistant process survived Stop")
	}
	if _, err := os.Stat(path); !os.IsNotExist(err) {
		t.Fatalf("pid file still present: %v", err)
	}
}

// TestHelperIgnoreSIGTERM only does anything as a spawned child process.
func TestHelperIgnoreSIGTERM(t *testing.T) {
	if os.Getenv("LLM_GATEWAY_TEST_HELPER") != "sigterm-resistant" {
		return
	}
	// Ignore before signaling readiness so the parent can never race the setup.
	signal.Ignore(syscall.SIGTERM)
	helperSignalReady(t)
	time.Sleep(time.Minute)
}
