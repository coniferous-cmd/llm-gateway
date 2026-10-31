package main

import (
	"bytes"
	"os"
	"path/filepath"
	"strings"
	"testing"
)

func TestRunCLI(t *testing.T) {
	for _, tc := range []struct {
		name   string
		args   []string
		code   int
		stdout string
		stderr string
	}{
		{"help", []string{"--help"}, 0, "Available Commands:", ""},
		{"serve help", []string{"serve", "--help"}, 0, "--daemon", ""},
		{"unknown flag", []string{"--unknown"}, 2, "", "unknown flag"},
		{"unknown command", []string{"unknown"}, 2, "", "unknown command"},
		{"missing value", []string{"--config"}, 2, "", "flag needs an argument"},
		{"invalid bool", []string{"--daemon=invalid"}, 2, "", "invalid argument"},
		{"version", []string{"version"}, 0, "llm-gateway " + version, ""},
		{"single dash version", []string{"-version"}, 0, "llm-gateway " + version, ""},
		{"single dash config value", []string{"-config", "-version", "-check-config"}, 1, "", "read config"},
		{"single dash config equals", []string{"-config=missing", "-check-config"}, 1, "", "read config"},
		{"version bypasses startup", []string{"--version", "--daemon", "--stop", "--config", "missing"}, 0, "llm-gateway " + version, ""},
		{"keygen precedes version", []string{"--keygen", "--version", "--daemon", "--stop"}, 0, "identity_key:", ""},
		{"keygen", []string{"keygen"}, 0, "identity_key:", ""},
		{"daemon stop conflict", []string{"--daemon", "--stop"}, 1, "--daemon and --stop cannot be combined", ""},
		{"check conflict precedes config", []string{"--check-config", "--version", "--config", "missing"}, 1, "", "--check-config cannot be combined with --version"},
		{"check rejects daemon", []string{"check-config", "--daemon"}, 2, "", "unknown flag"},
		{"check rejects pid file", []string{"check-config", "--pid-file", "unused"}, 2, "", "unknown flag"},
		{"check rejects log file", []string{"check-config", "--log-file", "unused"}, 2, "", "unknown flag"},
		{"serve rejects legacy stop", []string{"serve", "--stop"}, 2, "", "unknown flag"},
		{"stop rejects config", []string{"stop", "--config", "unused"}, 2, "", "unknown flag"},
		{"keygen rejects config", []string{"keygen", "--config", "unused"}, 2, "", "unknown flag"},
		{"version rejects extra args", []string{"version", "extra"}, 2, "", "unknown command"},
		{"default foreground", []string{"--config", "missing"}, 1, "read config", ""},
		{"serve foreground", []string{"serve", "--config", "missing"}, 1, "read config", ""},
		{"serve daemon", []string{"serve", "--daemon", "--config", "missing"}, 1, "background start failed", ""},
		{"completion", []string{"completion", "bash"}, 0, "bash completion", ""},
	} {
		t.Run(tc.name, func(t *testing.T) {
			var stdout, stderr bytes.Buffer
			code := runCLI(tc.args, &stdout, &stderr)
			if code != tc.code || !strings.Contains(stdout.String(), tc.stdout) || !strings.Contains(stderr.String(), tc.stderr) {
				t.Fatalf("exit %d stdout=%s stderr=%s", code, &stdout, &stderr)
			}
			if tc.stderr == "" && stderr.Len() != 0 {
				t.Fatalf("unexpected stderr: %s", &stderr)
			}
			if tc.stdout == "" && stdout.Len() != 0 {
				t.Fatalf("unexpected stdout: %s", &stdout)
			}
		})
	}
}

func TestCheckConfigCommand(t *testing.T) {
	dir := t.TempDir()
	path := filepath.Join(dir, "config.yaml")
	if err := os.WriteFile(path, []byte("keys: [{token: secret, target: {base_url: 'http://127.0.0.1:1'}}]\n"), 0600); err != nil {
		t.Fatal(err)
	}
	for _, command := range []string{"check-config", "--check-config"} {
		var stdout, stderr bytes.Buffer
		if code := runCLI([]string{command, "--config", path}, &stdout, &stderr); code != 0 || stderr.Len() != 0 || !strings.Contains(stdout.String(), "configuration valid: "+path) {
			t.Fatalf("%s: stdout=%s stderr=%s", command, &stdout, &stderr)
		}
		stdout.Reset()
		stderr.Reset()
		if code := runCLI([]string{command, "--config", filepath.Join(dir, "missing")}, &stdout, &stderr); code != 1 || stdout.Len() != 0 || !strings.Contains(stderr.String(), "configuration check failed:") {
			t.Fatalf("%s: stdout=%s stderr=%s", command, &stdout, &stderr)
		}
	}
}

func TestStopCommand(t *testing.T) {
	for _, command := range []string{"stop", "--stop"} {
		var stdout, stderr bytes.Buffer
		path := filepath.Join(t.TempDir(), "missing.pid")
		if code := runCLI([]string{command, "--pid-file", path}, &stdout, &stderr); code != 1 || stderr.Len() != 0 || !strings.Contains(stdout.String(), "stop failed") || !strings.Contains(stdout.String(), path) {
			t.Fatalf("%s: stdout=%s stderr=%s", command, &stdout, &stderr)
		}
	}
}

func TestForegroundErrorClosesResources(t *testing.T) {
	dir := t.TempDir()
	path := filepath.Join(dir, "config.yaml")
	// An invalid pid file fails after opening the log file.
	if err := os.WriteFile(path, []byte("keys: [{token: secret, target: {base_url: 'http://127.0.0.1:1'}}]\n"), 0600); err != nil {
		t.Fatal(err)
	}
	pidPath, logPath := filepath.Join(dir, "gateway.pid"), filepath.Join(dir, "gateway.log")
	if err := os.WriteFile(pidPath, []byte("invalid pid"), 0600); err != nil {
		t.Fatal(err)
	}
	var stdout, stderr bytes.Buffer
	originalOut, originalErr := os.Stdout, os.Stderr
	code := runCLI([]string{"serve", "--config", path, "--pid-file", pidPath, "--log-file", logPath}, &stdout, &stderr)
	if code != 1 || stdout.Len() != 0 || stderr.Len() != 0 {
		t.Fatalf("exit %d stdout=%s stderr=%s", code, &stdout, &stderr)
	}
	if os.Stdout != originalOut || os.Stderr != originalErr {
		t.Fatal("command changed process output streams")
	}
	data, err := os.ReadFile(logPath)
	if err != nil || !strings.Contains(string(data), "configuration failed") || strings.Count(string(data), "\n") != 1 {
		t.Fatalf("log=%s err=%v", data, err)
	}
}
