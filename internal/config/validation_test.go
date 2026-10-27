package config

import (
	"fmt"
	"strings"
	"testing"

	"gopkg.in/yaml.v3"
)

const validCheckConfig = "keys:\n  - token: secret-token\n    target: {base_url: 'https://example.com'}\n"

func TestStrictFields(t *testing.T) {
	cases := []string{
		"typo: secret-value\n" + validCheckConfig,
		"keys: [{token: dev, typo: secret-value, target: {base_url: 'https://example.com'}}]",
		"keys: [{token: dev, target: {base_url: 'https://example.com', typo: secret-value}}]",
		"keys: [{token: dev, target: {base_url: 'https://example.com'}, models: [{id: m, name: m, typo: secret-value}]}]",
		"e2ee: {typo: secret-value}\n" + validCheckConfig,
		"e2ee: {peers: [{typo: secret-value}]}\n" + validCheckConfig,
		"conversation_log: {typo: secret-value}\n" + validCheckConfig,
	}
	for i, input := range cases {
		t.Run(fmt.Sprint(i), func(t *testing.T) {
			_, err := Parse([]byte(input))
			if err == nil || !strings.Contains(err.Error(), "unknown configuration field \"typo\"") || !strings.Contains(err.Error(), "line ") || strings.Contains(err.Error(), "secret-value") {
				t.Fatalf("error: %v", err)
			}
		})
	}
	// Rewrite is intentionally a sequence of strings, not a mapping schema.
	if _, err := Parse([]byte("keys: [{token: dev, target: {base_url: 'https://example.com', rewrite: [{typo: secret-value}]}}]")); err == nil || !strings.Contains(err.Error(), "must be a string") {
		t.Fatalf("rewrite error: %v", err)
	}
}

func TestConfigAliasesAndMerges(t *testing.T) {
	inputs := []string{
		"keys:\n - &entry {token: dev, target: {base_url: 'https://example.com'}}\n - *entry\n",
		"keys:\n - token: dev\n   target: &target {base_url: 'https://example.com', api_key: secret}\n - token: dev\n   target: {<<: *target, api_key: override}\n",
		"keys:\n - &entry {token: dev, target: {base_url: 'https://example.com'}, models: &models [{id: m, name: m}]}\n - <<: [*entry]\n   token: other\n   models: *models\n",
	}
	for _, input := range inputs {
		if _, err := Parse([]byte(input)); err != nil {
			t.Fatalf("%s: %v", input, err)
		}
	}
	if _, err := Parse([]byte("keys: &cycle [*cycle]")); err == nil || !strings.Contains(err.Error(), "cyclic") {
		t.Fatalf("cycle: %v", err)
	}
}

func TestListenerValidation(t *testing.T) {
	valid := []string{"", ":0", ":65535", "localhost:8080", "127.0.0.1:0", "[::1]:8080", "[fe80::1%eth0]:80", "gateway.invalid:1234"}
	invalid := []string{"localhost", ":abc", ":65536", ":-1", ":+1", "::1:80", "[::1:80", "[bad]:80", "[127.0.0.1]:80", " host:80", "host:80 ", "bad host:80", "bad/host:80", ":", "[::1%]:80"}
	for _, address := range valid {
		t.Run("valid/"+address, func(t *testing.T) {
			for _, e2ee := range []bool{false, true} {
				prefix := fmt.Sprintf("listen: %q\n", address)
				if e2ee {
					prefix = fmt.Sprintf("e2ee: {listen: %q, identity_key: AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=}\n", address)
				}
				if _, err := Parse([]byte(prefix + validCheckConfig)); err != nil {
					t.Fatal(err)
				}
			}
		})
	}
	for _, address := range invalid {
		t.Run("invalid/"+address, func(t *testing.T) {
			for _, e2ee := range []bool{false, true} {
				field := "listen"
				prefix := fmt.Sprintf("listen: %q\n", address)
				if e2ee {
					field = "e2ee.listen"
					prefix = fmt.Sprintf("e2ee: {listen: %q}\n", address)
				}
				if _, err := Parse([]byte(prefix + validCheckConfig)); err == nil || !strings.Contains(err.Error(), field) || !strings.Contains(err.Error(), "line ") {
					t.Fatalf("%v", err)
				}
			}
		})
	}
}

func TestConfigDiagnosticsDoNotExposeSecrets(t *testing.T) {
	inputs := []string{
		"keys: secret-value",
		"keys: [{token: {password: secret-value}, target: {base_url: 'https://example.com'}}]",
		"keys: [{token: secret-value, target: {base_url: 'https://example.com'}, models: [{id: m, name: m}, {id: m, name: m}]}]",
		"e2ee: {identity_key: secret-value}\n" + validCheckConfig,
		"keys: [{token: dev, target: {base_url: 'https://example.com', expire: 9999999999999999999999999999999}}]",
	}
	for _, input := range inputs {
		_, err := Parse([]byte(input))
		if err == nil || strings.Contains(err.Error(), "secret-value") || strings.Contains(err.Error(), "9999999999999999999999999999999") {
			t.Fatalf("unsafe error: %v", err)
		}
	}
	// Verify yaml's scalar decode errors are redacted while line information remains.
	err := safeDecodeError(&yaml.TypeError{Errors: []string{"line 3: cannot unmarshal !!str `secret-value` into int"}})
	if strings.Contains(err.Error(), "secret-value") || !strings.Contains(err.Error(), "line 3") {
		t.Fatal(err)
	}
}
