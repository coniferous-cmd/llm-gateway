package e2ee

import (
	"bytes"
	"errors"
	"net"
	"strings"
	"sync"
	"testing"
)

// pipe returns an in-memory bidirectional connection pair.
func pipe(t *testing.T) (client, server net.Conn) {
	t.Helper()
	c, s := net.Pipe()
	t.Cleanup(func() { c.Close(); s.Close() })
	return c, s
}

func mustIdentity(t *testing.T) *Identity {
	t.Helper()
	id, err := GenerateIdentity()
	if err != nil {
		t.Fatalf("GenerateIdentity: %v", err)
	}
	return id
}

// handshakePair runs both sides of the IK handshake over a fresh pipe and
// returns the two sessions. The raw conns stay usable for wire-level attack
// simulations in tests.
func handshakePair(t *testing.T, clientID, serverID *Identity, allow func([]byte) bool) (clientConn, serverConn net.Conn, clientSession, serverSession *Session, serverErr error) {
	t.Helper()
	clientConn, serverConn = pipe(t)
	var wg sync.WaitGroup
	var cErr error
	wg.Add(1)
	go func() {
		defer wg.Done()
		clientSession, cErr = HandshakeInitiator(clientConn, clientID, serverID.Public.Public)
	}()
	serverSession, serverErr = HandshakeResponder(serverConn, serverID, allow)
	wg.Wait()
	if cErr != nil && serverErr == nil {
		t.Fatalf("HandshakeInitiator: %v", cErr)
	}
	return clientConn, serverConn, clientSession, serverSession, serverErr
}

func acceptAll([]byte) bool { return true }

func TestHandshakeSuccess(t *testing.T) {
	_, _, client, server, serverErr := handshakePair(t, mustIdentity(t), mustIdentity(t), acceptAll)
	if serverErr != nil {
		t.Fatalf("HandshakeResponder: %v", serverErr)
	}
	if client == nil || server == nil {
		t.Fatal("both sides must yield a session")
	}
}

func TestHandshakeWrongPinnedKey(t *testing.T) {
	clientID := mustIdentity(t)
	serverID := mustIdentity(t)
	impostor := mustIdentity(t) // client pins the wrong public key
	clientConn, serverConn := pipe(t)
	var wg sync.WaitGroup
	var clientErr error
	wg.Add(1)
	go func() {
		defer wg.Done()
		_, clientErr = HandshakeInitiator(clientConn, clientID, impostor.Public.Public)
	}()
	_, serverErr := HandshakeResponder(serverConn, serverID, acceptAll)
	wg.Wait()
	if clientErr == nil {
		t.Fatal("initiator must fail with a wrong pinned responder key")
	}
	if serverErr == nil {
		t.Fatal("responder must fail when the reply cannot authenticate")
	}
	if _, err := clientConn.Write([]byte("application data")); err == nil {
		t.Fatal("writing application data must fail after a rejected handshake")
	}
}

func TestHandshakeUnknownInitiatorRejected(t *testing.T) {
	clientID := mustIdentity(t)
	serverID := mustIdentity(t)
	clientConn, serverConn := pipe(t)
	var wg sync.WaitGroup
	var clientErr error
	wg.Add(1)
	go func() {
		defer wg.Done()
		_, clientErr = HandshakeInitiator(clientConn, clientID, serverID.Public.Public)
	}()
	// Empty allowlist: this client is not a configured peer.
	_, serverErr := HandshakeResponder(serverConn, serverID, func([]byte) bool { return false })
	wg.Wait()
	if serverErr == nil {
		t.Fatal("responder must reject an initiator absent from the allowlist")
	}
	if clientErr == nil {
		t.Fatal("initiator must observe the rejection (reply never arrives)")
	}
	if !strings.Contains(serverErr.Error(), "allowlist") {
		t.Fatalf("error should name the allowlist rejection, got: %v", serverErr)
	}
}

func TestSessionRoundTrip(t *testing.T) {
	clientID, serverID := mustIdentity(t), mustIdentity(t)
	_, _, client, server, serverErr := handshakePair(t, clientID, serverID, acceptAll)
	if serverErr != nil {
		t.Fatalf("handshake: %v", serverErr)
	}
	payload := []byte("GET /v1/chat/completions HTTP/1.1\r\nAuthorization: Bearer sk-real\r\n\r\n")

	errc := make(chan error, 1)
	go func() {
		buf := make([]byte, 4096)
		n, err := server.Read(buf)
		if err != nil {
			errc <- err
			return
		}
		if !bytes.Equal(buf[:n], payload) {
			errc <- errors.New("payload mismatch")
			return
		}
		_, err = server.Write([]byte("HTTP/1.1 200 OK\r\n\r\n"))
		errc <- err
	}()
	if _, err := client.Write(payload); err != nil {
		t.Fatalf("client Write: %v", err)
	}
	buf := make([]byte, 4096)
	n, err := client.Read(buf)
	if err != nil {
		t.Fatalf("client Read: %v", err)
	}
	if string(buf[:n]) != "HTTP/1.1 200 OK\r\n\r\n" {
		t.Fatalf("reply = %q", buf[:n])
	}
	if err := <-errc; err != nil {
		t.Fatalf("server side: %v", err)
	}
}

func TestSessionLargePayloadSplits(t *testing.T) {
	clientID, serverID := mustIdentity(t), mustIdentity(t)
	_, _, client, server, serverErr := handshakePair(t, clientID, serverID, acceptAll)
	if serverErr != nil {
		t.Fatalf("handshake: %v", serverErr)
	}
	// Payload spans multiple frames (well beyond one per-frame plaintext limit).
	payload := bytes.Repeat([]byte("streaming-chunk!"), MaxPayloadPerFrame/8+1)
	go func() {
		if _, err := client.Write(payload); err != nil {
			t.Errorf("client Write: %v", err)
		}
	}()
	var assembled []byte
	buf := make([]byte, 65536)
	for len(assembled) < len(payload) {
		n, err := server.Read(buf)
		if err != nil {
			t.Fatalf("server Read after %d bytes: %v", len(assembled), err)
		}
		assembled = append(assembled, buf[:n]...)
	}
	if !bytes.Equal(assembled, payload) {
		t.Fatalf("assembled %d bytes, want %d", len(assembled), len(payload))
	}
}

func TestSessionTamperRejected(t *testing.T) {
	clientID, serverID := mustIdentity(t), mustIdentity(t)
	clientConn, _, client, server, serverErr := handshakePair(t, clientID, serverID, acceptAll)
	if serverErr != nil {
		t.Fatalf("handshake: %v", serverErr)
	}
	// net.Pipe is synchronous, so the legitimate write must run concurrently
	// with the reads below.
	go func() {
		if _, err := client.Write([]byte("ok")); err != nil {
			t.Errorf("write legit frame: %v", err)
		}
		// Forged frame written directly on the wire, bypassing the session,
		// simulating an active on-path attacker.
		forged := []byte{0x00, 0x06, 'f', 'o', 'r', 'g', 'e', 'd'}
		if _, err := clientConn.Write(forged); err != nil {
			t.Errorf("write forged frame: %v", err)
		}
	}()
	buf := make([]byte, 64)
	n, err := server.Read(buf)
	if err != nil || string(buf[:n]) != "ok" {
		t.Fatalf("legit frame: got %q err %v", buf[:n], err)
	}
	if _, err := server.Read(buf); err == nil {
		t.Fatal("forged frame must fail authentication")
	}
}
