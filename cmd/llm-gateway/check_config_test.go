package main

import (
	"bytes"
	"context"
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

func TestCLICheckConfig(t *testing.T) {
	dir := t.TempDir()
	binary := buildBinary(t, dir)
	valid := "keys: [{token: secret-token, target: {base_url: 'http://127.0.0.1:1'}}]\n"
	run := func(env []string, args ...string) (string, string, int) {
		t.Helper()
		ctx, cancel := context.WithTimeout(context.Background(), 10*time.Second)
		defer cancel()
		cmd := exec.CommandContext(ctx, binary, args...)
		cmd.Env = append(os.Environ(), env...)
		var stdout, stderr bytes.Buffer
		cmd.Stdout, cmd.Stderr = &stdout, &stderr
		err := cmd.Run()
		if ctx.Err() != nil {
			t.Fatalf("command did not exit promptly: %v", ctx.Err())
		}
		code := 0
		if err != nil {
			exit, ok := err.(*exec.ExitError)
			if !ok {
				t.Fatal(err)
			}
			code = exit.ExitCode()
		}
		return stdout.String(), stderr.String(), code
	}
	writeConfig := func(data string) string {
		t.Helper()
		path := filepath.Join(dir, "config.yaml")
		if err := os.WriteFile(path, []byte(data), 0600); err != nil {
			t.Fatal(err)
		}
		return path
	}
	t.Run("explicit path and ignored resources", func(t *testing.T) {
		configPath := writeConfig(valid)
		pid, log := filepath.Join(dir, "absent", "gateway.pid"), filepath.Join(dir, "absent", "gateway.log")
		stdout, stderr, code := run(nil, "--check-config", "--config", configPath, "--pid-file", pid, "--log-file", log)
		if code != 0 || stderr != "" || !strings.Contains(stdout, "configuration valid:") {
			t.Fatalf("exit %d stdout=%s stderr=%s", code, stdout, stderr)
		}
		for _, path := range []string{pid, log, filepath.Dir(pid)} {
			if _, err := os.Stat(path); !os.IsNotExist(err) {
				t.Fatalf("resource touched: %s: %v", path, err)
			}
		}
	})
	t.Run("default path", func(t *testing.T) {
		home := filepath.Join(dir, "home")
		path := filepath.Join(home, ".config", "llm-gateway", "config.yaml")
		if err := os.MkdirAll(filepath.Dir(path), 0700); err != nil {
			t.Fatal(err)
		}
		if err := os.WriteFile(path, []byte(valid), 0600); err != nil {
			t.Fatal(err)
		}
		stdout, stderr, code := run([]string{"HOME=" + home, "USERPROFILE=" + home}, "--check-config")
		if code != 0 || stderr != "" || !strings.Contains(stdout, path) {
			t.Fatalf("exit %d stdout=%s stderr=%s", code, stdout, stderr)
		}
	})
	for _, operation := range []string{"--daemon", "--stop", "--keygen", "--version"} {
		t.Run("conflict/"+operation, func(t *testing.T) {
			stdout, stderr, code := run(nil, "--check-config", operation, "--config", filepath.Join(dir, "missing"))
			if code != 1 || stdout != "" || !strings.Contains(stderr, "cannot be combined with "+operation) {
				t.Fatalf("exit %d stdout=%s stderr=%s", code, stdout, stderr)
			}
		})
	}
	for _, tc := range []struct{ name, data, want string }{
		{"malformed YAML", "keys: [", "parse config"},
		{"semantic", "keys: []", "keys must not be empty"},
		{"unknown field", "typo: secret-value\n" + valid, "unknown configuration field"},
		{"invalid listener", "listen: ':65536'\n" + valid, "listen must be"},
		{"decode secret", "keys: secret-value", "decode config"},
	} {
		t.Run(tc.name, func(t *testing.T) {
			path := writeConfig(tc.data)
			stdout, stderr, code := run(nil, "--check-config", "--config", path)
			if code != 1 || stdout != "" || !strings.Contains(stderr, tc.want) || strings.Contains(stderr, "secret-value") {
				t.Fatalf("exit %d stdout=%s stderr=%s", code, stdout, stderr)
			}
			if tc.name == "unknown field" || tc.name == "invalid listener" {
				stdout, stderr, code = run(nil, "--config", path)
				if code != 1 || !strings.Contains(stdout+stderr, tc.want) {
					t.Fatalf("startup does not share validation: %d %s %s", code, stdout, stderr)
				}
			}
		})
	}
	t.Run("missing file", func(t *testing.T) {
		stdout, stderr, code := run(nil, "--check-config", "--config", filepath.Join(dir, "missing"))
		if code != 1 || stdout != "" || !strings.Contains(stderr, "read config") {
			t.Fatalf("exit %d stdout=%s stderr=%s", code, stdout, stderr)
		}
	})
	t.Run("occupied ports and no upstream or peer traffic", func(t *testing.T) {
		var requests atomic.Int32
		upstream := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) { requests.Add(1) }))
		defer upstream.Close()
		peer, err := net.Listen("tcp", "127.0.0.1:0")
		if err != nil {
			t.Fatal(err)
		}
		defer peer.Close()
		var connections atomic.Int32
		done := make(chan struct{})
		go func() {
			defer close(done)
			for {
				conn, err := peer.Accept()
				if err != nil {
					return
				}
				connections.Add(1)
				conn.Close()
			}
		}()
		conversationDir := filepath.Join(dir, "conversations")
		data := fmt.Sprintf("listen: %q\nconversation_log: {enabled: true, directory: %q}\ne2ee:\n  listen: %q\n  identity_key: AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=\n  peers: [{name: remote, base_url: %q, identity_pub: AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=}]\nkeys:\n - token: virtual\n   target: {base_url: %q}\n - token: encrypted\n   target: {peer: remote}\n", upstream.Listener.Addr().String(), filepath.ToSlash(conversationDir), peer.Addr().String(), "http://"+peer.Addr().String(), upstream.URL)
		path := writeConfig(data)
		pid, log := filepath.Join(dir, "existing.pid"), filepath.Join(dir, "existing.log")
		for _, file := range []string{pid, log} {
			if err := os.WriteFile(file, []byte("do not touch"), 0600); err != nil {
				t.Fatal(err)
			}
		}
		stdout, stderr, code := run(nil, "--check-config", "--config", path, "--pid-file", pid, "--log-file", log)
		peer.Close()
		<-done
		if code != 0 || stderr != "" || !strings.Contains(stdout, "configuration valid:") {
			t.Fatalf("exit %d stdout=%s stderr=%s", code, stdout, stderr)
		}
		if requests.Load() != 0 || connections.Load() != 0 {
			t.Fatal("check contacted an upstream or peer")
		}
		if _, err := os.Stat(conversationDir); !os.IsNotExist(err) {
			t.Fatalf("conversation directory touched: %v", err)
		}
		for _, file := range []string{pid, log} {
			data, err := os.ReadFile(file)
			if err != nil || string(data) != "do not touch" {
				t.Fatalf("file modified: %s %q %v", file, data, err)
			}
		}
	})
}
