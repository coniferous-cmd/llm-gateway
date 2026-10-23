package config

import (
	"fmt"
	"slices"
	"strings"
	"testing"
)

func TestParseRewrite(t *testing.T) {
	const prefix = "keys:\n  - token: dev\n    target:\n      base_url: https://example.com/\n"
	for _, tt := range []struct {
		name, value string
		want        []RewriteRule
	}{
		{"omitted", "", nil},
		{"empty", "      rewrite: []\n", nil},
		{"delete prefix", "      rewrite: ['/api:-', '/anthropic/:-', '/:-', '/keep:/-']\n", []RewriteRule{{"/api", ""}, {"/anthropic/", ""}, {"/", ""}, {"/keep", "/-"}}},
		{"ordered", "      rewrite: ['/api:/v1', '/openai:/v1', '/api:/v2']\n", []RewriteRule{{"/api", "/v1"}, {"/openai", "/v1"}, {"/api", "/v2"}}},
		{"root and trailing slash", "      rewrite: ['/:/v1/', '/api/:/']\n", []RewriteRule{{"/", "/v1/"}, {"/api/", "/"}}},
		{"escaped literals", "      rewrite: ['/a%2fb:/v%20x', '/%61pi:/v%25']\n", []RewriteRule{{"/a%2fb", "/v%20x"}, {"/%61pi", "/v%25"}}},
	} {
		t.Run(tt.name, func(t *testing.T) {
			cfg, err := Parse([]byte(prefix + tt.value))
			if err != nil {
				t.Fatal(err)
			}
			target := cfg.Keys["dev"][0].Target
			if target.BaseURL != "https://example.com" || !slices.Equal(target.Rewrite, tt.want) {
				t.Fatalf("target = %+v, want rules %+v", target, tt.want)
			}
		})
	}
}

func TestParseRewriteInvalid(t *testing.T) {
	for _, tt := range []struct {
		value, field string
		line         int
	}{
		{"'/api:/v1'", "", 5},
		{"{}", "", 5},
		{"null", "", 5},
		{"[42]", "[0]", 5},
		{"[true]", "[0]", 5},
		{"[null]", "[0]", 5},
		{"[{}]", "[0]", 5},
		{"[['/api:/v1']]", "[0]", 5},
		{"['/api:/v1', '/bad']", "[1]", 5},
		{"\n        - '/api:/v1'\n        - '/bad'", "[1]", 7},
	} {
		t.Run(tt.value, func(t *testing.T) {
			yml := "keys:\n  - token: dev\n    target:\n      base_url: https://example.com\n      rewrite: " + tt.value + "\n"
			_, err := Parse([]byte(yml))
			for _, want := range []string{fmt.Sprintf("line %d:", tt.line), "keys[0].target.rewrite" + tt.field} {
				if err == nil || !strings.Contains(err.Error(), want) {
					t.Fatalf("error = %v, want %q", err, want)
				}
			}
		})
	}
	for _, rule := range []string{"", "/api", ":/v1", "/api:", "api:/v1", "/api:v1", "/api:https://example.com", "//api:/v1", "/api://v1", "/api:/v1?x=1", "/api#x:/v1", "/a b:/v1", "/a%GG:/v1", "/api:/v%", "/api:/v1:extra", "/api:/a\\b", "/api:--", "/api: -", "/api:- ", "-:/v1", "//api:-", "/a%GG:-"} {
		t.Run(rule, func(t *testing.T) {
			yml := "keys:\n  - token: first\n    target: {base_url: https://first.example}\n  - token: dev\n    target:\n      base_url: https://example.com\n      rewrite:\n        - '" + rule + "'\n"
			_, err := Parse([]byte(yml))
			if err == nil || !strings.Contains(err.Error(), "line 8: keys[1].target.rewrite[0]") {
				t.Fatalf("error = %v", err)
			}
		})
	}
}

func TestParsePeerRewrite(t *testing.T) {
	yml := "e2ee:\n  identity_key: " + testIdentityKey + "\n  peers:\n    - name: remote\n      base_url: http://peer.example:8081\n      identity_pub: " + testIdentityPubA + "\nkeys:\n  - token: dev\n    target:\n      peer: remote\n      rewrite: ['/api:-']\n"
	cfg, err := Parse([]byte(yml))
	if err != nil {
		t.Fatal(err)
	}
	target := cfg.Keys["dev"][0].Target
	if target.Peer != "remote" || target.BaseURL != "http://peer.example:8081" || !slices.Equal(target.Rewrite, []RewriteRule{{"/api", ""}}) {
		t.Fatalf("peer target = %+v", target)
	}
}

func TestExampleConfig(t *testing.T) {
	if _, err := Load("../../config.example.yaml"); err != nil {
		t.Fatal(err)
	}
}
