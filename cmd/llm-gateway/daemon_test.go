package main

import (
	"fmt"
	"net"
	"net/http"
	"os"
	"os/exec"
	"path/filepath"
	"runtime"
	"strconv"
	"strings"
	"testing"
	"time"
)

// buildBinary compiles the gateway into dir for black-box CLI tests.
func buildBinary(t *testing.T, dir string) string {
	t.Helper()
	binary := filepath.Join(dir, "llm-gateway")
	if runtime.GOOS == "windows" {
		binary += ".exe"
	}
	build := exec.Command("go", "build", "-o", binary, ".")
	if output, err := build.CombinedOutput(); err != nil {
		t.Fatalf("build: %v\n%s", err, output)
	}
	return binary
}

// Exercise --daemon, --stop, --log-file and --pid-file with the real binary.
func TestDaemonStop(t *testing.T) {
	dir := t.TempDir()
	binary := buildBinary(t, dir)
	logs := func() string {
		content, _ := os.ReadFile(filepath.Join(dir, "gateway.log"))
		return string(content)
	}
	if output, err := exec.Command(binary, "--daemon", "--stop").CombinedOutput(); err == nil {
		t.Fatalf("conflicting flags accepted:\n%s", output)
	}
	listener, err := net.Listen("tcp", "127.0.0.1:0")
	if err != nil {
		t.Fatal(err)
	}
	address := listener.Addr().String()
	listener.Close()
	configPath := filepath.Join(dir, "config.yaml")
	data := fmt.Sprintf("listen: %q\nkeys:\n  - token: virtual\n    target: {base_url: \"http://127.0.0.1:1\"}\n", address)
	if err := os.WriteFile(configPath, []byte(data), 0600); err != nil {
		t.Fatal(err)
	}
	pidFile := filepath.Join(dir, "gateway.pid")
	logPath := filepath.Join(dir, "gateway.log")
	daemonArgs := []string{"--daemon", "--config", configPath, "--pid-file", pidFile, "--log-file", logPath}
	output, err := exec.Command(binary, daemonArgs...).CombinedOutput()
	if err != nil {
		t.Fatalf("daemon start: %v\n%s\nlogs:\n%s", err, output, logs())
	}
	if !strings.Contains(string(output), "started in background") {
		t.Fatalf("daemon start output:\n%s", output)
	}
	pidData, err := os.ReadFile(pidFile)
	if err != nil {
		t.Fatalf("pid file: %v\nlogs:\n%s", err, logs())
	}
	pid, err := strconv.Atoi(strings.TrimSpace(string(pidData)))
	if err != nil || pid <= 0 {
		t.Fatalf("pid file contents %q", pidData)
	}
	t.Logf("gateway runs as pid %d", pid)
	client := &http.Client{Timeout: 250 * time.Millisecond}
	defer client.CloseIdleConnections()
	deadline := time.Now().Add(5 * time.Second)
	for {
		resp, err := client.Get("http://" + address + "/ready")
		if err == nil {
			resp.Body.Close()
			if resp.StatusCode != http.StatusUnauthorized {
				t.Fatalf("readiness status = %d, want 401", resp.StatusCode)
			}
			break
		}
		if time.Now().After(deadline) {
			t.Fatalf("gateway not serving: %v\nlogs:\n%s", err, logs())
		}
		time.Sleep(50 * time.Millisecond)
	}
	output, err = exec.Command(binary, daemonArgs...).CombinedOutput()
	if err == nil || !strings.Contains(string(output), "already running") {
		t.Fatalf("second daemon start: %v\n%s\nlogs:\n%s", err, output, logs())
	}
	if output, err := exec.Command(binary, "--stop", "--pid-file", pidFile).CombinedOutput(); err != nil {
		t.Fatalf("stop: %v\n%s\nlogs:\n%s", err, output, logs())
	}
	if _, err := os.Stat(pidFile); !os.IsNotExist(err) {
		t.Fatalf("pid file still present after stop: %v", err)
	}
	deadline = time.Now().Add(5 * time.Second)
	for {
		conn, err := net.DialTimeout("tcp", address, 250*time.Millisecond)
		if err != nil {
			break
		}
		conn.Close()
		if time.Now().After(deadline) {
			t.Fatalf("gateway still accepting connections after stop\nlogs:\n%s", logs())
		}
		time.Sleep(50 * time.Millisecond)
	}
	output, err = exec.Command(binary, "--stop", "--pid-file", pidFile).CombinedOutput()
	if err == nil || !strings.Contains(string(output), "not running") {
		t.Fatalf("second stop: %v\n%s", err, output)
	}
}
