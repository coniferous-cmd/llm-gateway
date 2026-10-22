package e2ee

import (
	"fmt"
	"io"
)

// GenerateKeyPair writes a fresh identity keypair in the exact YAML shapes
// used by e2ee.identity_key (private, stays on this gateway) and
// e2ee.peers[].identity_pub (public, hand to the peer operator out of band).
func GenerateKeyPair(out io.Writer) error {
	id, err := GenerateIdentity()
	if err != nil {
		return err
	}
	_, err = fmt.Fprintf(out, "identity_key: %s\nidentity_pub: %s\n", id.EncodePrivate(), id.EncodePublic())
	return err
}
