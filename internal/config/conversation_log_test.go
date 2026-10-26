package config

import (
	"os"
	"path/filepath"
	"strings"
	"testing"
)

const conversationBase = "keys:\n  - token: test\n    target:\n      base_url: http://localhost\n"

func TestConversationLogConfig(t *testing.T) {
	for _, tc := range []struct {
		name, extra string
		bad         bool
	}{
		{"omitted", "", false}, {"enabled", "conversation_log: {enabled: true}", false},
		{"disabled", "conversation_log: {enabled: false}", false},
		{"relative", "conversation_log: {directory: my-logs, max_body_bytes: 1}", false},
		{"blank", "conversation_log: {directory: ' '}", true},
		{"zero", "conversation_log: {max_body_bytes: 0}", true},
		{"negative", "conversation_log: {max_body_bytes: -1}", true},
		{"string-limit", "conversation_log: {max_body_bytes: '3'}", true},
		{"fraction", "conversation_log: {max_body_bytes: 1.5}", true},
		{"bool", "conversation_log: {enabled: 'true'}", true},
		{"mapping", "conversation_log: []", true},
		{"dir-type", "conversation_log: {directory: 3}", true},
	} {
		t.Run(tc.name, func(t *testing.T) {
			cfg, err := Parse([]byte(conversationBase + tc.extra))
			if tc.bad {
				if err == nil || !strings.Contains(err.Error(), "conversation_log") {
					t.Fatalf("error = %v", err)
				}
				return
			}
			if err != nil {
				t.Fatal(err)
			}
			if tc.name == "omitted" && (cfg.ConversationLog.Enabled || cfg.ConversationLog.MaxBodyBytes != DefaultMaxBodyBytes || cfg.ConversationLog.Directory != "logs/conversations") {
				t.Fatalf("%+v", cfg.ConversationLog)
			}
			if tc.name == "enabled" && !cfg.ConversationLog.Enabled {
				t.Fatal("not enabled")
			}
		})
	}
}

func TestConversationLogPath(t *testing.T) {
	dir := t.TempDir()
	path := filepath.Join(dir, "config.yaml")
	for _, location := range []string{"relative", filepath.Join(dir, "absolute")} {
		text := conversationBase + "conversation_log:\n  enabled: true\n  directory: '" + filepath.ToSlash(location) + "'\n"
		if err := os.WriteFile(path, []byte(text), 0600); err != nil {
			t.Fatal(err)
		}
		cfg, err := Load(path)
		if err != nil {
			t.Fatal(err)
		}
		want := location
		if !filepath.IsAbs(location) {
			want = filepath.Join(dir, location)
		}
		if filepath.Clean(cfg.ConversationLog.Directory) != filepath.Clean(want) {
			t.Fatalf("got %q want %q", cfg.ConversationLog.Directory, want)
		}
		if _, err := os.Stat(want); !os.IsNotExist(err) {
			t.Fatalf("parse touched storage: %v", err)
		}
	}
}
