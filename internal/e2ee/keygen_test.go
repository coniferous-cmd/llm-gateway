package e2ee

import (
	"bytes"
	"strings"
	"testing"
)

func TestGenerateKeyPairOutput(t *testing.T) {
	var out bytes.Buffer
	if err := GenerateKeyPair(&out); err != nil {
		t.Fatalf("GenerateKeyPair: %v", err)
	}
	lines := strings.Split(strings.TrimSpace(out.String()), "\n")
	if len(lines) != 2 {
		t.Fatalf("output must be exactly two lines, got %d: %q", len(lines), out.String())
	}
	if !strings.HasPrefix(lines[0], "identity_key: ") || !strings.HasPrefix(lines[1], "identity_pub: ") {
		t.Fatalf("output shape = %q", out.String())
	}
	priv := strings.TrimPrefix(lines[0], "identity_key: ")
	pub := strings.TrimPrefix(lines[1], "identity_pub: ")
	if priv == "" || pub == "" || priv == pub {
		t.Fatalf("private/public must be non-empty and distinct: %q / %q", priv, pub)
	}
	// The private line must parse back into a working identity whose public
	// half matches the printed public line.
	id, err := ParseIdentity(priv)
	if err != nil {
		t.Fatalf("ParseIdentity(generated): %v", err)
	}
	if id.EncodePublic() != pub {
		t.Fatalf("derived public %q != printed public %q", id.EncodePublic(), pub)
	}
}
