package main

import (
	"bufio"
	"fmt"
	"io"
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

// Model routing through the real binary: one token, two entries with disjoint
// model ids; the body's model picks the provider and aliases are rewritten.
func TestCLIModelRouting(t *testing.T) {
	curl, err := exec.LookPath("curl")
	if err != nil {
		t.Skip("curl is required for CLI end-to-end verification")
	}
	var flashCalls, proCalls atomic.Int64
	var flashBody, proBody atomic.Value
	mock := func(name string, calls *atomic.Int64, body *atomic.Value) *httptest.Server {
		return httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
			calls.Add(1)
			data, _ := io.ReadAll(r.Body)
			body.Store(string(data))
			if r.Header.Get("Authorization") != "Bearer real-"+name {
				t.Errorf("wrong authorization at %s", name)
			}
			_, _ = fmt.Fprint(w, name)
		}))
	}
	flash, pro := mock("flash", &flashCalls, &flashBody), mock("pro", &proCalls, &proBody)
	defer flash.Close()
	defer pro.Close()
	dir := t.TempDir()
	binary := buildBinary(t, dir)
	listener, err := net.Listen("tcp", "127.0.0.1:0")
	if err != nil {
		t.Fatal(err)
	}
	address := listener.Addr().String()
	listener.Close()
	config := filepath.Join(dir, "config.yaml")
	data := fmt.Sprintf("listen: %q\nkeys:\n  - token: virtual\n    target: {base_url: %q, api_key: real-flash}\n    models:\n      - id: mimo-v2.6-flash\n        name: mimo-v2.6-flash\n  - token: virtual\n    target: {base_url: %q, api_key: real-pro}\n    models:\n      - id: alias-pro\n        name: mimo-v2.6-pro\n", address, flash.URL, pro.URL)
	if err := os.WriteFile(config, []byte(data), 0600); err != nil {
		t.Fatal(err)
	}
	logPath := filepath.Join(dir, "gateway.log")
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
	defer func() {
		_ = process.Process.Kill()
		_ = process.Wait()
		if t.Failed() {
			logs, _ := os.ReadFile(logPath)
			t.Logf("gateway logs:\n%s", logs)
		}
	}()
	baseURL := "http://" + address
	client := &http.Client{Timeout: 250 * time.Millisecond}
	defer client.CloseIdleConnections()
	deadline := time.Now().Add(10 * time.Second)
	for {
		resp, err := client.Get(baseURL + "/ready")
		if err == nil {
			resp.Body.Close()
			if resp.StatusCode != 401 {
				t.Fatal("readiness request was not rejected")
			}
			break
		}
		if time.Now().After(deadline) {
			t.Fatalf("gateway did not start: %v", err)
		}
		time.Sleep(25 * time.Millisecond)
	}
	curlCommand := func(args ...string) *exec.Cmd {
		return exec.Command(curl, append([]string{"--noproxy", "*", "--silent", "--show-error", "--max-time", "10"}, args...)...)
	}
	completions := func(body string) (string, string) {
		t.Helper()
		responsePath := filepath.Join(dir, "response.bin")
		status, err := curlCommand("--output", responsePath, "--write-out", "%{http_code}", "-H", "Authorization: Bearer virtual", "-H", "Content-Type: application/json", "--data-binary", body, baseURL+"/v1/chat/completions").CombinedOutput()
		if err != nil {
			t.Fatalf("curl %s: %v", body, err)
		}
		data, err := os.ReadFile(responsePath)
		if err != nil {
			t.Fatal(err)
		}
		return string(data), string(status)
	}
	// The model id selects the entry; id == name keeps the body untouched.
	body := `{"model":"mimo-v2.6-flash","n":1}`
	if response, status := completions(body); status != "200" || response != "flash" || flashBody.Load() != body {
		t.Fatalf("flash: status %s, response %q, upstream body %q", status, response, flashBody.Load())
	}
	// The alias is rewritten to the provider's model name before forwarding.
	if response, status := completions(`{"model":"alias-pro"}`); status != "200" || response != "pro" || proBody.Load() != `{"model":"mimo-v2.6-pro"}` {
		t.Fatalf("pro: status %s, response %q, upstream body %q", status, response, proBody.Load())
	}
	// An unknown model id is rejected without contacting an upstream.
	if _, status := completions(`{"model":"typo"}`); status != "400" || flashCalls.Load()+proCalls.Load() != 2 {
		t.Fatalf("unknown model: status %s, calls %d/%d", status, flashCalls.Load(), proCalls.Load())
	}
	t.Log("curl verified: model-based routing, alias rewrite and 400 rejection")
}

// Exercise the real binary, YAML loading, --config and curl over TCP.
func TestCLIEndToEnd(t *testing.T) {
	curl, err := exec.LookPath("curl")
	if err != nil {
		t.Skip("curl is required for CLI end-to-end verification")
	}
	var calls atomic.Int64
	release := make(chan struct{})
	mock := func(name string) *httptest.Server {
		return httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
			calls.Add(1)
			if expectedHost := r.Context().Value(http.LocalAddrContextKey).(net.Addr).String(); r.Host != expectedHost {
				t.Errorf("upstream Host = %q, want %q", r.Host, expectedHost)
			}
			if r.Header.Get("Authorization") != "Bearer real-"+name {
				t.Errorf("wrong authorization at %s", name)
			}
			if r.URL.Path == "/stream" {
				w.Header().Set("Content-Type", "text/event-stream")
				_, _ = io.WriteString(w, "data: first\n\n")
				w.(http.Flusher).Flush()
				select {
				case <-release:
				case <-r.Context().Done():
					return
				}
				_, _ = io.WriteString(w, "data: second\n\n")
				return
			}
			data, _ := io.ReadAll(r.Body)
			_, _ = fmt.Fprintf(w, "%s %s %s %s", name, r.Method, r.RequestURI, data)
		}))
	}
	first, second := mock("first"), mock("second")
	defer first.Close()
	defer second.Close()
	defer close(release)
	dir := t.TempDir()
	binary := buildBinary(t, dir)
	listener, err := net.Listen("tcp", "127.0.0.1:0")
	if err != nil {
		t.Fatal(err)
	}
	address := listener.Addr().String()
	listener.Close()
	config := filepath.Join(dir, "config.yaml")
	data := fmt.Sprintf("listen: %q\nkeys:\n  - token: virtual\n    target: {base_url: %q, api_key: real-first}\n  - token: virtual\n    target: {base_url: %q, api_key: real-second}\n", address, first.URL+"/", second.URL)
	if err := os.WriteFile(config, []byte(data), 0600); err != nil {
		t.Fatal(err)
	}
	logPath := filepath.Join(dir, "gateway.log")
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
	defer func() {
		_ = process.Process.Kill()
		_ = process.Wait()
		if t.Failed() {
			logs, _ := os.ReadFile(logPath)
			t.Logf("gateway logs:\n%s", logs)
		}
	}()
	baseURL := "http://" + address
	client := &http.Client{Timeout: 250 * time.Millisecond}
	defer client.CloseIdleConnections()
	deadline := time.Now().Add(10 * time.Second)
	for {
		resp, err := client.Get(baseURL + "/ready")
		if err == nil {
			resp.Body.Close()
			if resp.StatusCode != 401 {
				t.Fatal("readiness request was not rejected")
			}
			break
		}
		if time.Now().After(deadline) {
			t.Fatalf("gateway did not start: %v", err)
		}
		time.Sleep(25 * time.Millisecond)
	}
	curlCommand := func(args ...string) *exec.Cmd {
		return exec.Command(curl, append([]string{"--noproxy", "*", "--silent", "--show-error", "--max-time", "10"}, args...)...)
	}
	rejected, err := curlCommand("--output", os.DevNull, "--write-out", "%{http_code}", "-H", "Authorization: Bearer wrong", baseURL+"/v1/chat/completions").CombinedOutput()
	if err != nil || string(rejected) != "401" || calls.Load() != 0 {
		t.Fatalf("unauthorized request: %q %v, calls %d", rejected, err, calls.Load())
	}
	for i, name := range []string{"first", "second", "first", "second"} {
		path := "/v1/chat/completions?stream=false"
		if i >= 2 {
			path = "/arbitrary/a%2Fb?raw=%2f"
		}
		output, err := curlCommand("-H", "Authorization: Bearer virtual", "-H", "Content-Type: text/plain", "--data-binary", "opaque-body", baseURL+path).CombinedOutput()
		want := name + " POST " + path + " opaque-body"
		if err != nil || string(output) != want {
			t.Fatalf("curl response: %q, want %q; %v", output, want, err)
		}
		t.Logf("curl verified: %s", output)
	}
	stream := curlCommand("--no-buffer", "-H", "Authorization: Bearer virtual", baseURL+"/stream")
	stdout, err := stream.StdoutPipe()
	if err != nil {
		t.Fatal(err)
	}
	if err := stream.Start(); err != nil {
		t.Fatal(err)
	}
	defer func() { _ = stream.Process.Kill() }()
	reader := bufio.NewReader(stdout)
	line, err := reader.ReadString('\n')
	if err != nil || line != "data: first\n" {
		t.Fatalf("first SSE chunk: %q %v", line, err)
	}
	// The upstream cannot finish until this point; receiving the first event proves flushing.
	release <- struct{}{}
	rest, err := io.ReadAll(reader)
	if err != nil || !strings.Contains(string(rest), "data: second\n") {
		t.Fatalf("second SSE chunk: %q %v", rest, err)
	}
	if err := stream.Wait(); err != nil {
		t.Fatal(err)
	}
	t.Log("curl verified: first SSE event arrived before upstream was allowed to finish")
}
