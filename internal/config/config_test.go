package config

import (
	"os"
	"path/filepath"
	"slices"
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
			if cfg.Keys["dev"][0].Target.BaseURL != "https://example.com" {
				t.Fatal(cfg.Keys)
			}
		})
	}
}

func TestParseModels(t *testing.T) {
	entry := func(models string) string {
		return "keys:\n  - token: dev\n    target: {base_url: 'https://example.com'}\n" + models
	}
	tests := []struct {
		name, yaml, wantError string
		want                  []Model
	}{
		{"declared", entry("    models:\n      - id: mimo-v2.6-flash\n        name: mimo-v2.6-flash\n      - id: mimo-v2.6-pro\n        name: mimo-pro\n"), "", []Model{{ID: "mimo-v2.6-flash", Name: "mimo-v2.6-flash"}, {ID: "mimo-v2.6-pro", Name: "mimo-pro"}}},
		{"legacy without models", entry(""), "", nil},
		{"empty list", entry("    models: []\n"), "", nil},
		{"missing id", entry("    models:\n      - name: n\n"), "models[0].id", nil},
		{"empty id", entry("    models:\n      - id: '  '\n        name: n\n"), "models[0].id", nil},
		{"missing name", entry("    models:\n      - id: m\n"), "models[0].name", nil},
		{"empty name", entry("    models:\n      - id: m\n        name: ''\n"), "models[0].name", nil},
		{"duplicate id in one entry", entry("    models:\n      - id: m\n        name: a\n      - id: m\n        name: b\n"), "\"m\"", nil},
		{"duplicate id across entries", entry("    models:\n      - id: mimo-v2.6-flash\n        name: m\n") + "  - token: dev\n    target: {base_url: 'https://other.com'}\n    models:\n      - id: mimo-v2.6-flash\n        name: renamed\n", "already declared", nil},
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
			if !slices.Equal(cfg.Keys["dev"][0].Models, tt.want) {
				t.Fatalf("models = %#v, want %#v", cfg.Keys["dev"][0].Models, tt.want)
			}
		})
	}
}

func TestParseSameModelIDForDifferentTokens(t *testing.T) {
	yaml := "keys:\n  - token: first\n    target: {base_url: 'https://example.com'}\n    models: [{id: m, name: m}]\n  - token: second\n    target: {base_url: 'https://other.com'}\n    models: [{id: m, name: m}]\n"
	cfg, err := Parse([]byte(yaml))
	if err != nil {
		t.Fatal(err)
	}
	if len(cfg.Keys["first"]) != 1 || len(cfg.Keys["second"]) != 1 {
		t.Fatal(cfg.Keys)
	}
}

func TestDefaultPath(t *testing.T) {
	home, err := os.UserHomeDir()
	if err != nil {
		t.Fatal(err)
	}
	path, err := DefaultPath()
	if err != nil {
		t.Fatal(err)
	}
	want := filepath.Join(home, ".config", "llm-gateway", "config.yaml")
	if path != want {
		t.Fatalf("DefaultPath() = %q, want %q", path, want)
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
	if cfg.Listen != "127.0.0.1:9090" || cfg.Keys["dev"][0].Target.APIKey != "" {
		t.Fatal(cfg)
	}
}

// 32-byte base64 keys; the config parser checks shape, not key validity.
const (
	testIdentityKey  = "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA="
	testIdentityPubA = "AQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQE="
	testIdentityPubB = "AgICAgICAgICAgICAgICAgAgICAgICAgICAgICAgICAg="
)

func TestParseE2EE(t *testing.T) {
	const prefix = "e2ee:\n" +
		"  listen: \"0.0.0.0:9081\"\n" +
		"  identity_key: " + testIdentityKey + "\n" +
		"  peers:\n" +
		"    - name: gateway-c\n" +
		"      base_url: \"http://10.0.0.2:9081\"\n" +
		"      identity_pub: " + testIdentityPubA + "\n"

	t.Run("valid peer target", func(t *testing.T) {
		yaml := prefix + "keys:\n  - token: dev\n    target:\n      peer: gateway-c\n      api_key: sk-virt\n"
		cfg, err := Parse([]byte(yaml))
		if err != nil {
			t.Fatal(err)
		}
		if cfg.E2EE == nil || cfg.E2EE.Listen != "0.0.0.0:9081" || len(cfg.E2EE.Peers) != 1 {
			t.Fatalf("e2ee = %+v", cfg.E2EE)
		}
		target := cfg.Keys["dev"][0].Target
		if target.Peer != "gateway-c" || target.BaseURL != "http://10.0.0.2:9081" {
			t.Fatalf("target must resolve peer to its base_url: %+v", target)
		}
	})

	t.Run("valid plaintext target alongside e2ee", func(t *testing.T) {
		yaml := prefix + "keys:\n  - token: dev\n    target:\n      base_url: \"https://api.openai.com\"\n"
		cfg, err := Parse([]byte(yaml))
		if err != nil {
			t.Fatal(err)
		}
		if cfg.Keys["dev"][0].Target.Peer != "" {
			t.Fatal("plaintext target must not carry a peer")
		}
	})

	t.Run("no e2ee section still valid", func(t *testing.T) {
		yaml := "keys:\n  - token: dev\n    target:\n      base_url: https://example.com\n"
		cfg, err := Parse([]byte(yaml))
		if err != nil {
			t.Fatal(err)
		}
		if cfg.E2EE != nil {
			t.Fatal("E2EE must be nil when not configured")
		}
	})

	t.Run("base_url and peer mutually exclusive", func(t *testing.T) {
		yaml := prefix + "keys:\n  - token: dev\n    target:\n      base_url: \"https://x.com\"\n      peer: gateway-c\n"
		if _, err := Parse([]byte(yaml)); err == nil || !strings.Contains(err.Error(), "exactly one of base_url or peer") {
			t.Fatalf("error = %v", err)
		}
	})

	t.Run("neither base_url nor peer", func(t *testing.T) {
		yaml := "keys:\n  - token: dev\n    target:\n      api_key: k\n"
		if _, err := Parse([]byte(yaml)); err == nil || !strings.Contains(err.Error(), "must set one of base_url or peer") {
			t.Fatalf("error = %v", err)
		}
	})

	t.Run("undeclared peer reference", func(t *testing.T) {
		yaml := prefix + "keys:\n  - token: dev\n    target:\n      peer: ghost\n"
		if _, err := Parse([]byte(yaml)); err == nil || !strings.Contains(err.Error(), "not declared in e2ee.peers") {
			t.Fatalf("error = %v", err)
		}
	})

	t.Run("peer target without e2ee section", func(t *testing.T) {
		yaml := "keys:\n  - token: dev\n    target:\n      peer: gateway-c\n"
		if _, err := Parse([]byte(yaml)); err == nil || !strings.Contains(err.Error(), "not declared in e2ee.peers") {
			t.Fatalf("error = %v", err)
		}
	})

	t.Run("missing identity_key", func(t *testing.T) {
		yaml := "e2ee:\n  listen: \":9081\"\nkeys:\n  - token: dev\n    target:\n      base_url: https://example.com\n"
		if _, err := Parse([]byte(yaml)); err == nil || !strings.Contains(err.Error(), "e2ee.identity_key is required") {
			t.Fatalf("error = %v", err)
		}
	})

	t.Run("malformed identity_key", func(t *testing.T) {
		bad := strings.Replace(prefix, testIdentityKey, "short", 1)
		bad += "keys:\n  - token: dev\n    target:\n      base_url: https://example.com\n"
		if _, err := Parse([]byte(bad)); err == nil || !strings.Contains(err.Error(), "identity_key must be") {
			t.Fatalf("error = %v", err)
		}
	})

	t.Run("malformed identity_pub", func(t *testing.T) {
		bad := strings.Replace(prefix, testIdentityPubA, "nope!", 1)
		bad += "keys:\n  - token: dev\n    target:\n      base_url: https://example.com\n"
		if _, err := Parse([]byte(bad)); err == nil || !strings.Contains(err.Error(), "identity_pub must be") {
			t.Fatalf("error = %v", err)
		}
	})

	t.Run("duplicate peer name", func(t *testing.T) {
		yaml := "e2ee:\n" +
			"  identity_key: " + testIdentityKey + "\n" +
			"  peers:\n" +
			"    - name: gateway-c\n      base_url: \"http://10.0.0.2:9081\"\n      identity_pub: " + testIdentityPubA + "\n" +
			"    - name: gateway-c\n      base_url: \"http://10.0.0.3:9081\"\n      identity_pub: " + testIdentityPubB + "\n" +
			"keys:\n  - token: dev\n    target:\n      base_url: https://example.com\n"
		if _, err := Parse([]byte(yaml)); err == nil || !strings.Contains(err.Error(), "duplicated") {
			t.Fatalf("error = %v", err)
		}
	})

	t.Run("peer address collides with plaintext target", func(t *testing.T) {
		yaml := "e2ee:\n" +
			"  identity_key: " + testIdentityKey + "\n" +
			"  peers:\n" +
			"    - name: gateway-c\n      base_url: \"http://10.0.0.2:9081\"\n      identity_pub: " + testIdentityPubA + "\n" +
			"keys:\n  - token: dev\n    target:\n      base_url: http://10.0.0.2:9081\n"
		if _, err := Parse([]byte(yaml)); err == nil || !strings.Contains(err.Error(), "collides") {
			t.Fatalf("error = %v", err)
		}
	})

	t.Run("error line numbers", func(t *testing.T) {
		yaml := prefix + "keys:\n  - token: dev\n    target:\n      peer: ghost\n"
		_, err := Parse([]byte(yaml))
		if err == nil || !strings.HasPrefix(err.Error(), "line 11:") {
			t.Fatalf("error = %v, want line 11", err)
		}
	})

	t.Run("default e2ee listen", func(t *testing.T) {
		yaml := "e2ee:\n" +
			"  identity_key: " + testIdentityKey + "\n" +
			"keys:\n  - token: dev\n    target:\n      base_url: https://example.com\n"
		cfg, err := Parse([]byte(yaml))
		if err != nil {
			t.Fatal(err)
		}
		if cfg.E2EE.Listen != DefaultE2EEListen {
			t.Fatalf("listen = %q, want %q", cfg.E2EE.Listen, DefaultE2EEListen)
		}
	})
}
