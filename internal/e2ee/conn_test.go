package e2ee

import (
	"bufio"
	"bytes"
	"context"
	"io"
	"log/slog"
	"net"
	"testing"
	"time"
)

// e2eePair starts a wrapped listener on a random loopback port and returns a
// dialer for it, so tests exercise the real TCP + handshake path.
func e2eePair(t *testing.T, clientID, serverID *Identity, allowed map[string][]byte) (*Dialer, string) {
	t.Helper()
	raw, err := net.Listen("tcp", "127.0.0.1:0")
	if err != nil {
		t.Fatalf("listen: %v", err)
	}
	wrapped := NewListener(raw, serverID, allowed, slog.New(slog.DiscardHandler))
	t.Cleanup(func() { wrapped.Close() })
	go func() {
		for {
			conn, err := wrapped.Accept()
			if err != nil {
				return
			}
			// Echo server: copy everything back, byte for byte.
			go func(c net.Conn) {
				defer c.Close()
				_, _ = io.Copy(c, c)
			}(conn)
		}
	}()
	dialer := NewDialer(clientID, map[string][]byte{raw.Addr().String(): serverID.Public.Public})
	return dialer, raw.Addr().String()
}

func TestDialAndEcho(t *testing.T) {
	clientID, serverID := mustIdentity(t), mustIdentity(t)
	dialer, addr := e2eePair(t, clientID, serverID, map[string][]byte{"client": clientID.Public.Public})

	conn, err := dialer.DialContext(context.Background(), "tcp", addr)
	if err != nil {
		t.Fatalf("DialContext: %v", err)
	}
	defer conn.Close()
	payload := []byte("GET /v1/models HTTP/1.1\r\nAuthorization: Bearer sk-virt\r\n\r\n")
	if _, err := conn.Write(payload); err != nil {
		t.Fatalf("write: %v", err)
	}
	got := make([]byte, len(payload))
	if _, err := io.ReadFull(conn, got); err != nil {
		t.Fatalf("read echo: %v", err)
	}
	if !bytes.Equal(got, payload) {
		t.Fatalf("echo = %q", got)
	}
}

func TestDialRefusesUnconfiguredAddress(t *testing.T) {
	dialer := NewDialer(mustIdentity(t), map[string][]byte{})
	if dialer.IsPeer("example.com:8081") {
		t.Fatal("IsPeer must be false for unknown addresses")
	}
	if _, err := dialer.DialContext(context.Background(), "tcp", "example.com:8081"); err == nil {
		t.Fatal("dialing a non-peer address must fail")
	}
}

func TestListenerRejectsUnknownIdentity(t *testing.T) {
	clientID, serverID := mustIdentity(t), mustIdentity(t)
	stranger := mustIdentity(t)
	// Server allowlist contains only stranger; real client must be rejected.
	raw, err := net.Listen("tcp", "127.0.0.1:0")
	if err != nil {
		t.Fatalf("listen: %v", err)
	}
	acceptedc := make(chan net.Conn, 1)
	// Allowlist contains only the stranger's key; the real client must never
	// be handed out as a session. Accept loops past rejections, so keep it
	// running and assert the channel stays empty.
	wrapped := NewListener(raw, serverID, map[string][]byte{"stranger": stranger.Public.Public}, slog.New(slog.DiscardHandler))
	t.Cleanup(func() { wrapped.Close() })
	go func() {
		for {
			conn, err := wrapped.Accept()
			if err != nil {
				close(acceptedc)
				return
			}
			acceptedc <- conn
		}
	}()
	dialer := NewDialer(clientID, map[string][]byte{raw.Addr().String(): serverID.Public.Public})
	conn, err := dialer.DialContext(context.Background(), "tcp", raw.Addr().String())
	if err == nil {
		conn.Close()
		t.Fatal("dial must fail when the server rejects our identity")
	}
	// The rejected connection must never surface as an accepted session.
	select {
	case accepted, ok := <-acceptedc:
		if ok && accepted != nil {
			t.Fatal("listener must not hand out a session for a rejected identity")
		}
	case <-time.After(500 * time.Millisecond):
		// Still listening, still no accepted session: correct.
	}
}

func TestSSEChunksArriveIncrementally(t *testing.T) {
	clientID, serverID := mustIdentity(t), mustIdentity(t)
	raw, err := net.Listen("tcp", "127.0.0.1:0")
	if err != nil {
		t.Fatalf("listen: %v", err)
	}
	wrapped := NewListener(raw, serverID, map[string][]byte{"client": clientID.Public.Public}, slog.New(slog.DiscardHandler))
	t.Cleanup(func() { wrapped.Close() })
	// Barrier proving incremental delivery: the producer will not write event
	// N+1 until the consumer has fully read event N. If the session buffered
	// the whole response, the first read would block forever and the test
	// would time out.
	consumed := make(chan struct{})
	produced := make(chan struct{})
	events := []string{"data: first\n\n", "data: second\n\n", "data: third\n\n"}
	go func() {
		conn, err := wrapped.Accept()
		if err != nil {
			close(produced)
			return
		}
		defer conn.Close()
		for _, event := range events {
			if _, err := conn.Write([]byte(event)); err != nil {
				break
			}
			if _, ok := <-consumed; !ok {
				break
			}
		}
		close(produced)
	}()
	dialer := NewDialer(clientID, map[string][]byte{raw.Addr().String(): serverID.Public.Public})
	conn, err := dialer.DialContext(context.Background(), "tcp", raw.Addr().String())
	if err != nil {
		t.Fatalf("dial: %v", err)
	}
	defer conn.Close()
	_ = conn.SetDeadline(time.Now().Add(10 * time.Second))
	reader := bufio.NewReader(conn)
	for i, want := range events {
		got, err := reader.ReadString('\n')
		if err != nil {
			t.Fatalf("chunk %d line: %v", i, err)
		}
		blank, err := reader.ReadString('\n')
		if err != nil {
			t.Fatalf("chunk %d terminator: %v", i, err)
		}
		if got+blank != want {
			t.Fatalf("chunk %d = %q, want %q", i, got+blank, want)
		}
		consumed <- struct{}{} // let the producer send the next event
	}
	<-produced
}
