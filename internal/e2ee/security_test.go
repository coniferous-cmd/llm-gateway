package e2ee

import (
	"bytes"
	"context"
	"encoding/hex"
	"log/slog"
	"net"
	"strings"
	"testing"
)

// A rejected handshake must keep private key bytes out of the logs. Public
// keys are not secret (operators paste them into config), but a private key
// must never appear in any encoding.
func TestLogsNeverContainPrivateKeyMaterial(t *testing.T) {
	clientID, serverID := mustIdentity(t), mustIdentity(t)
	stranger := mustIdentity(t)

	var logs bytes.Buffer
	logger := slog.New(slog.NewTextHandler(&logs, nil))

	raw, err := net.Listen("tcp", "127.0.0.1:0")
	if err != nil {
		t.Fatal(err)
	}
	// Allowlist holds only the stranger: the real client is rejected.
	wrapped := NewListener(raw, serverID, map[string][]byte{"stranger": stranger.Public.Public}, logger)
	t.Cleanup(func() { wrapped.Close() })
	done := make(chan struct{})
	go func() {
		defer close(done)
		for {
			conn, err := wrapped.Accept()
			if err != nil {
				return
			}
			conn.Close()
		}
	}()

	dialer := NewDialer(clientID, map[string][]byte{raw.Addr().String(): serverID.Public.Public})
	conn, err := dialer.DialContext(context.Background(), "tcp", raw.Addr().String())
	if err == nil {
		conn.Close()
		t.Fatal("handshake must fail: client is not in the allowlist")
	}

	// The client fails as soon as the rejection is sent, but the accept loop
	// may still be writing the rejection log. Close the listener and join the
	// goroutine so every log write happens-before the buffer is read below.
	wrapped.Close()
	<-done

	// The rejection log may name the remote address and the failure, but must
	// not echo private key bytes in any encoding.
	for _, secret := range []string{
		clientID.EncodePrivate(),
		serverID.EncodePrivate(),
		hex.EncodeToString(clientID.Private.Private),
		hex.EncodeToString(serverID.Private.Private),
	} {
		if strings.Contains(logs.String(), secret) {
			t.Fatalf("logs leaked key material %q:\n%s", secret, logs.String())
		}
	}
	if !strings.Contains(logs.String(), "e2ee handshake rejected") {
		t.Fatalf("rejection must be logged, got:\n%s", logs.String())
	}
}
