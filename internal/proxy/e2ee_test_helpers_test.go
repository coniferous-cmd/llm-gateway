package proxy

import (
	"log/slog"
	"net"
	"net/http"
	"net/http/httptest"
	"testing"

	"llm-gateway/internal/config"
	"llm-gateway/internal/e2ee"
	"llm-gateway/internal/keymap"
)

// e2eeIdentities generates a client and peer identity for tests.
func e2eeIdentities() (client, peer *e2ee.Identity, err error) {
	client, err = e2ee.GenerateIdentity()
	if err != nil {
		return nil, nil, err
	}
	peer, err = e2ee.GenerateIdentity()
	return client, peer, err
}

// startE2EEServer wraps a TCP listener in the responder handshake and serves
// the given plain HTTP handler over the decrypted sessions. Returns the
// listener's host:port and a stop function.
func startE2EEServer(t *testing.T, identity *e2ee.Identity, allow map[string][]byte, handler http.Handler) (addr string, stop func()) {
	t.Helper()
	raw, err := net.Listen("tcp", "127.0.0.1:0")
	if err != nil {
		t.Fatalf("listen: %v", err)
	}
	wrapped := e2ee.NewListener(raw, identity, allow, slog.New(slog.DiscardHandler))
	srv := &http.Server{Handler: handler}
	go func() { _ = srv.Serve(wrapped) }()
	return raw.Addr().String(), func() {
		srv.Close()
		wrapped.Close()
	}
}

// e2eeDialer builds a client-side Noise dialer for one peer address.
func e2eeDialer(identity *e2ee.Identity, addr string, pinnedPub []byte) *e2ee.Dialer {
	return e2ee.NewDialer(identity, map[string][]byte{addr: pinnedPub})
}

// gatewayWithPeers is like gateway() but with a peer dialer table wired into
// the proxy transport.
func gatewayWithPeers(t *testing.T, target config.Target, peers map[string]PeerDialer) *httptest.Server {
	t.Helper()
	p := New(peers)
	s := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		p.ServeHTTP(w, r.WithContext(keymap.WithTarget(r.Context(), target)))
	}))
	t.Cleanup(s.Close)
	return s
}
