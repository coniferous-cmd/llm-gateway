package main

import (
	"encoding/json"
	"fmt"
	"io"
	"log/slog"
	"net/http"
	"net/http/httptest"
	"os"
	"os/exec"
	"path/filepath"
	"strings"
	"testing"
	"time"

	"llm-gateway/internal/config"
	"llm-gateway/internal/conversationlog"
)

func waitConversationRecord(t *testing.T, dir string) conversationlog.Record {
	t.Helper()
	deadline := time.Now().Add(5 * time.Second)
	for {
		files, err := filepath.Glob(filepath.Join(dir, "*", "*.json"))
		if err != nil {
			t.Fatal(err)
		}
		if len(files) == 1 {
			b, err := os.ReadFile(files[0])
			if err != nil {
				t.Fatal(err)
			}
			var r conversationlog.Record
			if err = json.Unmarshal(b, &r); err != nil {
				t.Fatal(err)
			}
			return r
		}
		if time.Now().After(deadline) {
			t.Fatalf("no unique record in %s: %v", dir, files)
		}
		time.Sleep(10 * time.Millisecond)
	}
}
func TestBuildConversationRecorderDisabled(t *testing.T) {
	dir := filepath.Join(t.TempDir(), "not-created")
	cfg := &config.Config{ConversationLog: config.ConversationLog{Directory: dir, MaxBodyBytes: 8}}
	if buildConversationRecorder(cfg, slog.Default()) != nil {
		t.Fatal("enabled unexpectedly")
	}
	if _, err := os.Stat(dir); !os.IsNotExist(err) {
		t.Fatal(err)
	}
}
func TestDaemonConversationConfigRelative(t *testing.T) {
	dir := t.TempDir()
	binary := buildBinary(t, dir)
	upstream := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) { _, _ = io.Copy(w, r.Body) }))
	defer upstream.Close()
	listen := freeAddress(t)
	configPath := filepath.Join(dir, "config.yaml")
	cfg := fmt.Sprintf("listen: %q\nkeys:\n  - token: virtual\n    target: {base_url: %q}\nconversation_log: {enabled: true, directory: conversations}\n", listen, upstream.URL)
	if err := os.WriteFile(configPath, []byte(cfg), 0600); err != nil {
		t.Fatal(err)
	}
	pid := filepath.Join(dir, "gateway.pid")
	log := filepath.Join(dir, "operational.log")
	cmd := exec.Command(binary, "--daemon", "--config", configPath, "--pid-file", pid, "--log-file", log)
	cmd.Dir = t.TempDir()
	if out, err := cmd.CombinedOutput(); err != nil {
		t.Fatalf("start: %v %s", err, out)
	}
	t.Cleanup(func() { _ = exec.Command(binary, "--stop", "--pid-file", pid).Run() })
	client := &http.Client{Timeout: 2 * time.Second}
	defer client.CloseIdleConnections()
	deadline := time.Now().Add(5 * time.Second)
	for {
		resp, err := client.Get("http://" + listen + "/ready")
		if err == nil {
			_ = resp.Body.Close()
			break
		}
		if time.Now().After(deadline) {
			t.Fatal(err)
		}
		time.Sleep(10 * time.Millisecond)
	}
	req, _ := http.NewRequest("POST", "http://"+listen+"/chat", strings.NewReader("daemon-body"))
	req.Header.Set("Authorization", "Bearer virtual")
	resp, err := client.Do(req)
	if err != nil {
		t.Fatal(err)
	}
	body, err := io.ReadAll(resp.Body)
	_ = resp.Body.Close()
	if err != nil || string(body) != "daemon-body" {
		t.Fatalf("%q %v", body, err)
	}
	record := waitConversationRecord(t, filepath.Join(dir, "conversations"))
	if record.Request.Data != "daemon-body" || record.Response.Data != "daemon-body" {
		t.Fatal(record)
	}
	logs, err := os.ReadFile(log)
	if err != nil {
		t.Fatal(err)
	}
	if !strings.Contains(string(logs), record.RequestID) || strings.Contains(string(logs), "daemon-body") {
		t.Fatalf("operational log: %s", logs)
	}
}
