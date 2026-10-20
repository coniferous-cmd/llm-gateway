package config

import (
	"os"
	"path/filepath"
	"strings"
	"testing"
)

func TestParse(t *testing.T) {
	tests := []struct {
		name, yaml, wantError string
		count                 int
	}{
		{"valid", "keys:\n  - token: dev\n    target:\n      base_url: https://example.com\n      api_key: real\n", "", 1},
		{"empty", "", "keys", 0},
		{"no keys", "listen: localhost:8080", "keys", 0},
		{"missing token", "keys:\n  - target:\n      base_url: https://example.com", "line 2", 0},
		{"missing target", "keys:\n  - token: dev", "base_url", 0},
		{"invalid URL", "keys:\n  - token: dev\n    target: {base_url: 'not-a-url'}", "line 3", 0},
		{"no host", "keys:\n  - token: dev\n    target: {base_url: 'https://'}", "base_url", 0},
		{"wrong scheme", "keys:\n  - token: dev\n    target: {base_url: 'ftp://example.com'}", "base_url", 0},
		{"path", "keys:\n  - token: dev\n    target: {base_url: 'https://example.com/v1'}", "base_url", 0},
		{"query", "keys:\n  - token: dev\n    target: {base_url: 'https://example.com?x=1'}", "base_url", 0},
		{"fragment", "keys:\n  - token: dev\n    target: {base_url: 'https://example.com#x'}", "base_url", 0},
		{"credentials", "keys:\n  - token: dev\n    target: {base_url: 'https://user:pass@example.com'}", "base_url", 0},
		{"slash", "keys:\n  - token: dev\n    target: {base_url: 'https://example.com/'}", "", 1},
		{"merge", "keys:\n  - token: dev\n    target: {base_url: 'https://example.com'}\n  - token: dev\n    target: {base_url: 'http://localhost:11434/'}", "", 2},
		{"bad yaml", "keys: [", "parse config", 0},
		{"wrong type", "keys: nope", "decode config", 0},
	}
	for _, tt := range tests {
		t.Run(tt.name, func(t *testing.T) {
			cfg, err := Parse([]byte(tt.yaml))
			if tt.wantError != "" {
				if err == nil || !strings.Contains(err.Error(), tt.wantError) {
					t.Fatalf("error = %v, want %q", err, tt.wantError)
				}
				return
			}
			if err != nil {
				t.Fatal(err)
			}
			if cfg.Listen != DefaultListen || len(cfg.Keys["dev"]) != tt.count {
				t.Fatalf("unexpected config: %+v", cfg)
			}
			if cfg.Keys["dev"][0].BaseURL != "https://example.com" {
				t.Fatal(cfg.Keys)
			}
		})
	}
}

func TestLoad(t *testing.T) {
	path := filepath.Join(t.TempDir(), "config.yaml")
	if _, err := Load(path); err == nil {
		t.Fatal("missing file accepted")
	}
	if err := os.WriteFile(path, []byte("listen: '127.0.0.1:9090'\nkeys:\n  - token: dev\n    target: {base_url: 'http://localhost:11434'}"), 0600); err != nil {
		t.Fatal(err)
	}
	cfg, err := Load(path)
	if err != nil {
		t.Fatal(err)
	}
	if cfg.Listen != "127.0.0.1:9090" || cfg.Keys["dev"][0].APIKey != "" {
		t.Fatal(cfg)
	}
}
