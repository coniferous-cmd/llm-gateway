package main

import (
	"fmt"
	"net"
	"net/http"
	"net/http/httptest"
	"os"
	"os/exec"
	"path/filepath"
	"strings"
	"sync/atomic"
	"testing"
	"time"
)

// freeAddress reserves a loopback port and returns its address for reuse.
func freeAddress(t *testing.T) string {
	t.Helper()
	ln, err := net.Listen("tcp", "127.0.0.1:0")
	if err != nil {
		t.Fatal(err)
	}
	addr := ln.Addr().String()
	ln.Close()
	return addr
}

// startGateway launches the real binary with the given config and waits until
// its plaintext listen address rejects an unauthorized probe with 401.
func startGateway(t *testing.T, binary, config, listen, logName string) string {
	t.Helper()
	logPath := filepath.Join(t.TempDir(), logName)
	logFile, err := os.Create(logPath)
	if err != nil {
		t.Fatal(err)
	}
	defer logFile.Close()
	process := exec.Command(binary, "--config", config)
	process.Stdout, process.Stderr = logFile, logFile
	if err := process.Start(); err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() {
		_ = process.Process.Kill()
		_ = process.Wait()
		if t.Failed() {
			logs, _ := os.ReadFile(logPath)
			t.Logf("%s logs:\n%s", logName, logs)
		}
	})
	baseURL := "http://" + listen
	client := &http.Client{Timeout: 250 * time.Millisecond}
	defer client.CloseIdleConnections()
	deadline := time.Now().Add(10 * time.Second)
	for {
		resp, err := client.Get(baseURL + "/ready")
		if err == nil {
			resp.Body.Close()
			if resp.StatusCode != 401 {
				t.Fatalf("%s readiness = %d, want 401", logName, resp.StatusCode)
			}
			return baseURL
		}
		if time.Now().After(deadline) {
			t.Fatalf("%s did not start: %v", logName, err)
		}
		time.Sleep(25 * time.Millisecond)
	}
}

// Two real gateway processes chained over the encrypted link: client -> B
// (plaintext) -> C (Noise IK) -> mock upstream. Verifies --keygen output works
// in config, peer target resolution, and the full encrypted hop end to end.
func TestCLIChainedGatewaysOverE2EE(t *testing.T) {
	var calls atomic.Int64
	var gotAuth, gotPath atomic.Value
	upstream := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		calls.Add(1)
		gotAuth.Store(r.Header.Get("Authorization"))
		gotPath.Store(r.URL.RequestURI())
		_, _ = fmt.Fprint(w, "through-c")
	}))
	defer upstream.Close()

	dir := t.TempDir()
	binary := buildBinary(t, dir)

	// Generate both identities with the real --keygen flag so the e2e test
	// also covers the operator workflow.
	keygen := func() (private, public string) {
		out, err := exec.Command(binary, "--keygen").Output()
		if err != nil {
			t.Fatalf("keygen: %v", err)
		}
		for _, line := range strings.Split(strings.TrimSpace(string(out)), "\n") {
			switch {
			case strings.HasPrefix(line, "identity_key: "):
				private = strings.TrimPrefix(line, "identity_key: ")
			case strings.HasPrefix(line, "identity_pub: "):
				public = strings.TrimPrefix(line, "identity_pub: ")
			}
		}
		if private == "" || public == "" {
			t.Fatalf("keygen output shape: %q", out)
		}
		return private, public
	}
	bPriv, bPub := keygen()
	cPriv, cPub := keygen()
	if bPriv == cPriv {
		t.Fatal("keygen must produce distinct keys")
	}

	bListen, bE2EE := freeAddress(t), freeAddress(t)
	cListen, cE2EE := freeAddress(t), freeAddress(t)

	cConfig := filepath.Join(dir, "c.yaml")
	cData := fmt.Sprintf(`listen: %q
e2ee:
  listen: %q
  identity_key: %s
  peers:
    - name: gateway-b
      base_url: "http://%s"
      identity_pub: %s
keys:
  - token: sk-virt-to-c
    target: {base_url: %q, api_key: real-upstream}
`, cListen, cE2EE, cPriv, bE2EE, bPub, upstream.URL)
	if err := os.WriteFile(cConfig, []byte(cData), 0600); err != nil {
		t.Fatal(err)
	}

	bConfig := filepath.Join(dir, "b.yaml")
	bData := fmt.Sprintf(`listen: %q
e2ee:
  listen: %q
  identity_key: %s
  peers:
    - name: gateway-c
      base_url: "http://%s"
      identity_pub: %s
keys:
  - token: virtual
    target: {peer: gateway-c, api_key: sk-virt-to-c}
`, bListen, bE2EE, bPriv, cE2EE, cPub)
	if err := os.WriteFile(bConfig, []byte(bData), 0600); err != nil {
		t.Fatal(err)
	}

	startGateway(t, binary, cConfig, cListen, "gateway-c.log")
	bURL := startGateway(t, binary, bConfig, bListen, "gateway-b.log")

	// Unauthorized probes never reach the chain.
	probeClient := &http.Client{Timeout: 2 * time.Second}
	probe, err := probeClient.Get(bURL + "/ready")
	if err != nil {
		t.Fatalf("probe: %v", err)
	}
	probe.Body.Close()
	if probe.StatusCode != 401 {
		t.Fatalf("probe status = %d, want 401", probe.StatusCode)
	}

	req, _ := http.NewRequest(http.MethodPost, bURL+"/v1/chat/completions?q=1", strings.NewReader(`{"model":"m"}`))
	req.Header.Set("Authorization", "Bearer virtual")
	req.Header.Set("Content-Type", "application/json")
	client := &http.Client{Timeout: 15 * time.Second}
	defer client.CloseIdleConnections()
	resp, err := client.Do(req)
	if err != nil {
		t.Fatalf("chained request: %v", err)
	}
	defer resp.Body.Close()
	body := make([]byte, 64)
	n, _ := resp.Body.Read(body)
	if resp.StatusCode != 200 || string(body[:n]) != "through-c" {
		t.Fatalf("status=%d body=%q", resp.StatusCode, body[:n])
	}
	if calls.Load() != 1 || gotAuth.Load() != "Bearer real-upstream" || gotPath.Load() != "/v1/chat/completions?q=1" {
		t.Fatalf("upstream: calls=%d auth=%v path=%v", calls.Load(), gotAuth.Load(), gotPath.Load())
	}
}

// --keygen exits 0 without starting a server and prints a pasteable pair.
func TestCLIKeygen(t *testing.T) {
	dir := t.TempDir()
	binary := buildBinary(t, dir)
	out, err := exec.Command(binary, "--keygen").Output()
	if err != nil {
		t.Fatalf("keygen: %v", err)
	}
	lines := strings.Split(strings.TrimSpace(string(out)), "\n")
	if len(lines) != 2 || !strings.HasPrefix(lines[0], "identity_key: ") || !strings.HasPrefix(lines[1], "identity_pub: ") {
		t.Fatalf("keygen output: %q", out)
	}
}
