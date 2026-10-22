package e2ee

import (
	"context"
	"fmt"
	"log/slog"
	"net"
	"time"
)

// Dialer dials peer gateways and wraps each TCP connection in a Noise IK
// session pinned to the peer's configured identity public key. One handshake
// runs per TCP connection; every request on that connection reuses the
// session (keep-alive), and closing the connection destroys its keys.
type Dialer struct {
	identity *Identity
	// peers maps "host:port" (the peer's e2ee listen address) to that peer's
	// pinned identity public key.
	peers map[string][]byte
	// dialer underlies every connection; injectable for tests.
	dialer *net.Dialer
}

// NewDialer builds a client-side dialer. identity is the local long-term
// keypair; peers maps peer addresses (host:port) to pinned public keys.
func NewDialer(identity *Identity, peers map[string][]byte) *Dialer {
	return &Dialer{
		identity: identity,
		peers:    peers,
		dialer:   &net.Dialer{Timeout: 10 * time.Second, KeepAlive: 30 * time.Second},
	}
}

// IsPeer reports whether addr (host:port) is a configured e2ee peer.
func (d *Dialer) IsPeer(addr string) bool {
	_, ok := d.peers[addr]
	return ok
}

// DialContext dials addr and completes the initiator handshake. It matches
// the net.Dialer.DialContext signature so it plugs straight into
// http.Transport.DialContext. addr must be a configured peer; anything else
// is refused so plaintext targets can never be dialed through by accident.
func (d *Dialer) DialContext(ctx context.Context, network, addr string) (net.Conn, error) {
	pinned, ok := d.peers[addr]
	if !ok {
		return nil, fmt.Errorf("address %s is not a configured e2ee peer", addr)
	}
	conn, err := d.dialer.DialContext(ctx, network, addr)
	if err != nil {
		return nil, fmt.Errorf("dial peer %s: %w", addr, err)
	}
	session, err := HandshakeInitiator(conn, d.identity, pinned)
	if err != nil {
		conn.Close()
		return nil, fmt.Errorf("e2ee handshake with %s: %w", addr, err)
	}
	return session, nil
}

// Listener wraps a TCP listener so every accepted connection must complete
// the responder handshake with an identity from the allowlist before any
// HTTP bytes are exchanged. Rejected identities are closed immediately.
type Listener struct {
	net.Listener
	identity *Identity
	allow    map[string]struct{} // pinned initiator public keys
	logger   *slog.Logger
}

// NewListener wraps ln. identity is the local keypair; allowedPeers maps
// pinned initiator public keys (raw bytes) to peer names for logging.
func NewListener(ln net.Listener, identity *Identity, allowedPeers map[string][]byte, logger *slog.Logger) *Listener {
	allow := make(map[string]struct{}, len(allowedPeers))
	for _, pub := range allowedPeers {
		allow[string(pub)] = struct{}{}
	}
	if logger == nil {
		logger = slog.Default()
	}
	return &Listener{Listener: ln, identity: identity, allow: allow, logger: logger}
}

// Accept blocks until a peer completes the handshake, returning the wrapped
// session as the connection. Handshake failures are logged and the raw
// connection is discarded; Accept keeps waiting for the next connection.
func (l *Listener) Accept() (net.Conn, error) {
	for {
		conn, err := l.Listener.Accept()
		if err != nil {
			return nil, err
		}
		session, err := HandshakeResponder(conn, l.identity, func(peer []byte) bool {
			_, ok := l.allow[string(peer)]
			return ok
		})
		if err != nil {
			l.logger.Warn("e2ee handshake rejected",
				"remote", conn.RemoteAddr().String(), "error", err)
			conn.Close()
			continue
		}
		l.logger.Info("e2ee session established", "remote", conn.RemoteAddr().String())
		return session, nil
	}
}
