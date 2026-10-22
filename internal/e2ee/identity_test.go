package e2ee

import (
	"encoding/base64"
	"strings"
	"testing"
)

func TestGenerateIdentityRoundTrip(t *testing.T) {
	id, err := GenerateIdentity()
	if err != nil {
		t.Fatalf("GenerateIdentity: %v", err)
	}
	if len(id.Private.Private) != IdentityKeySize || len(id.Public.Public) != IdentityKeySize {
		t.Fatalf("key sizes = %d/%d, want %d", len(id.Private.Private), len(id.Public.Public), IdentityKeySize)
	}
	parsed, err := ParseIdentity(id.EncodePrivate())
	if err != nil {
		t.Fatalf("ParseIdentity: %v", err)
	}
	if parsed.EncodePublic() != id.EncodePublic() {
		t.Fatal("public key derived from parsed private key does not match original")
	}
}

func TestParseIdentityErrors(t *testing.T) {
	short := base64.StdEncoding.EncodeToString(make([]byte, 16))
	valid, err := GenerateIdentity()
	if err != nil {
		t.Fatalf("GenerateIdentity: %v", err)
	}
	tests := []struct {
		name    string
		encoded string
	}{
		{"not base64", "!!!"},
		{"wrong length", short},
		{"empty", ""},
	}
	for _, tt := range tests {
		t.Run(tt.name, func(t *testing.T) {
			if _, err := ParseIdentity(tt.encoded); err == nil {
				t.Fatalf("ParseIdentity(%q) succeeded, want error", tt.encoded)
			}
		})
	}
	// Sanity: a generated key still parses (controls for the table above).
	if _, err := ParseIdentity(valid.EncodePrivate()); err != nil {
		t.Fatalf("ParseIdentity of fresh key: %v", err)
	}
}

func TestParsePublicKeyErrors(t *testing.T) {
	tests := []struct {
		name    string
		encoded string
	}{
		{"not base64", "not!base64"},
		{"wrong length", base64.StdEncoding.EncodeToString(make([]byte, 31))},
	}
	for _, tt := range tests {
		t.Run(tt.name, func(t *testing.T) {
			if _, err := ParsePublicKey(tt.encoded); err == nil {
				t.Fatalf("ParsePublicKey(%q) succeeded, want error", tt.encoded)
			}
		})
	}
}

func TestKeyEncodingPasteable(t *testing.T) {
	id, err := GenerateIdentity()
	if err != nil {
		t.Fatalf("GenerateIdentity: %v", err)
	}
	priv, pub := id.EncodePrivate(), id.EncodePublic()
	if priv == pub {
		t.Fatal("private and public encodings must differ")
	}
	if strings.ContainsAny(priv+pub, "\n\r ") {
		t.Fatal("encodings must be single-line pasteable")
	}
	// Must decode to exactly key-size bytes each.
	for _, encoded := range []string{priv, pub} {
		raw, err := base64.StdEncoding.DecodeString(encoded)
		if err != nil || len(raw) != IdentityKeySize {
			t.Fatalf("encoding %q: err=%v len=%d", encoded, err, len(raw))
		}
	}
}
