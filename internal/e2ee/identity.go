// Package e2ee encrypts the TCP byte stream between gateways with the Noise IK
// protocol: each gateway holds a long-term X25519 identity key, peers pin each
// other's identity public keys in config, and every connection negotiates a
// forward-secret session after a single handshake.
package e2ee

import (
	"crypto/ecdh"
	"crypto/rand"
	"encoding/base64"
	"fmt"

	"github.com/flynn/noise"
)

// IdentityKeySize is the byte length of an X25519 key.
const IdentityKeySize = 32

// Identity is one gateway's long-term X25519 keypair, in the raw form the
// Noise suite (DH25519) consumes.
type Identity struct {
	Private noise.DHKey
	Public  noise.DHKey
}

// GenerateIdentity creates a fresh X25519 identity keypair.
func GenerateIdentity() (*Identity, error) {
	key, err := ecdh.X25519().GenerateKey(rand.Reader)
	if err != nil {
		return nil, fmt.Errorf("generate identity keypair: %w", err)
	}
	return identityFromECDH(key), nil
}

// ParseIdentity decodes a base64-encoded X25519 private key and derives its
// public half. The input must be exactly IdentityKeySize bytes after decoding.
func ParseIdentity(encoded string) (*Identity, error) {
	raw, err := base64.StdEncoding.DecodeString(encoded)
	if err != nil {
		return nil, fmt.Errorf("decode identity key: %w", err)
	}
	key, err := ecdh.X25519().NewPrivateKey(raw)
	if err != nil {
		return nil, fmt.Errorf("identity key must be a valid %d-byte X25519 key: %w", IdentityKeySize, err)
	}
	return identityFromECDH(key), nil
}

// ParsePublicKey decodes a base64-encoded X25519 public key. Peers' pinned
// keys come from config and must fail fast when malformed.
func ParsePublicKey(encoded string) ([]byte, error) {
	raw, err := base64.StdEncoding.DecodeString(encoded)
	if err != nil {
		return nil, fmt.Errorf("decode identity public key: %w", err)
	}
	key, err := ecdh.X25519().NewPublicKey(raw)
	if err != nil {
		return nil, fmt.Errorf("identity public key must be a valid %d-byte X25519 key: %w", IdentityKeySize, err)
	}
	return key.Bytes(), nil
}

// EncodePrivate returns the base64 form for e2ee.identity_key in config.
func (id *Identity) EncodePrivate() string {
	return base64.StdEncoding.EncodeToString(id.Private.Private)
}

// EncodePublic returns the base64 form for e2ee.peers[].identity_pub in config.
func (id *Identity) EncodePublic() string {
	return base64.StdEncoding.EncodeToString(id.Public.Public)
}

func identityFromECDH(key *ecdh.PrivateKey) *Identity {
	public := key.PublicKey().Bytes()
	return &Identity{
		Private: noise.DHKey{Private: key.Bytes(), Public: public},
		Public:  noise.DHKey{Public: public},
	}
}
