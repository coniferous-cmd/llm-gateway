package e2ee

import (
	"encoding/binary"
	"fmt"
	"io"
	"net"
	"time"

	"github.com/flynn/noise"
)

// prologue binds every handshake to this protocol version so gateways speaking
// different envelope versions can never accidentally interoperate.
var prologue = []byte("llm-gateway-e2ee/1")

// Frame sizes follow the Noise spec: a frame is a 2-byte big-endian length
// followed by at most one Noise message (65535 bytes including the AEAD tag).
const (
	frameHeaderSize = 2
	maxFrameSize    = 65535
	tagSize         = 16 // ChaCha20-Poly1305
	// MaxPayloadPerFrame is the largest plaintext chunk one frame can carry.
	MaxPayloadPerFrame = maxFrameSize - tagSize
	// HandshakeTimeout bounds the one-time IK handshake on a new connection.
	HandshakeTimeout = 10 * time.Second
)

// Session is a completed IK handshake wrapped around the underlying
// connection. It implements net.Conn: every Write is sealed into one or more
// AEAD frames, every Read opens the next frame. Nonces advance per message
// inside each direction's CipherState, so replayed, reordered or spliced
// frames fail authentication.
type Session struct {
	conn net.Conn
	send *noise.CipherState
	recv *noise.CipherState
	buf  []byte // decrypted-but-unread bytes from the current frame
}

func cipherSuite() noise.CipherSuite {
	return noise.NewCipherSuite(noise.DH25519, noise.CipherChaChaPoly, noise.HashBLAKE2s)
}

// HandshakeInitiator runs the initiator (first-message) side of Noise IK over
// conn. peerPublic is the pinned identity public key of the responder: if the
// other end does not hold the matching private key, the handshake fails and no
// application data is ever exchanged.
func HandshakeInitiator(conn net.Conn, id *Identity, peerPublic []byte) (*Session, error) {
	if err := conn.SetDeadline(time.Now().Add(HandshakeTimeout)); err != nil {
		return nil, fmt.Errorf("set handshake deadline: %w", err)
	}
	hs, err := noise.NewHandshakeState(noise.Config{
		CipherSuite:   cipherSuite(),
		Pattern:       noise.HandshakeIK,
		Initiator:     true,
		Prologue:      prologue,
		StaticKeypair: id.Private,
		PeerStatic:    peerPublic,
	})
	if err != nil {
		return nil, fmt.Errorf("initiator handshake state: %w", err)
	}
	msg, _, _, err := hs.WriteMessage(nil, nil)
	if err != nil {
		return nil, fmt.Errorf("initiator handshake write: %w", err)
	}
	if err := writeFrame(conn, msg); err != nil {
		return nil, fmt.Errorf("send handshake message: %w", err)
	}
	msg, err = readFrame(conn)
	if err != nil {
		return nil, fmt.Errorf("read handshake reply: %w", err)
	}
	_, cs1, cs2, err := hs.ReadMessage(nil, msg)
	if err != nil {
		// A wrong pinned key or an impostor without the private key fails
		// here: the reply cannot authenticate under the derived session keys.
		return nil, fmt.Errorf("finish initiator handshake (peer identity rejected): %w", err)
	}
	if cs1 == nil || cs2 == nil {
		return nil, fmt.Errorf("initiator handshake did not complete")
	}
	if err := conn.SetDeadline(time.Time{}); err != nil {
		return nil, fmt.Errorf("clear handshake deadline: %w", err)
	}
	// cs1 carries initiator -> responder traffic, cs2 responder -> initiator.
	return &Session{conn: conn, send: cs1, recv: cs2}, nil
}

// HandshakeResponder runs the responder side of Noise IK over conn. allow is
// consulted with the initiator's decrypted identity public key; returning
// false aborts the handshake before the responder reveals any session state.
func HandshakeResponder(conn net.Conn, id *Identity, allow func(peerPublic []byte) bool) (*Session, error) {
	if err := conn.SetDeadline(time.Now().Add(HandshakeTimeout)); err != nil {
		return nil, fmt.Errorf("set handshake deadline: %w", err)
	}
	hs, err := noise.NewHandshakeState(noise.Config{
		CipherSuite:   cipherSuite(),
		Pattern:       noise.HandshakeIK,
		Initiator:     false,
		Prologue:      prologue,
		StaticKeypair: id.Private,
	})
	if err != nil {
		return nil, fmt.Errorf("responder handshake state: %w", err)
	}
	msg, err := readFrame(conn)
	if err != nil {
		return nil, fmt.Errorf("read handshake message: %w", err)
	}
	if _, _, _, err := hs.ReadMessage(nil, msg); err != nil {
		return nil, fmt.Errorf("read initiator message: %w", err)
	}
	// IK carries the initiator's static key encrypted to our identity; only a
	// holder of the legitimate client's private key produces a decryptable
	// message, so this static is authenticated.
	peer := hs.PeerStatic()
	if len(peer) == 0 || !allow(peer) {
		return nil, fmt.Errorf("initiator identity %x not in peer allowlist", peer)
	}
	reply, cs1, cs2, err := hs.WriteMessage(nil, nil)
	if err != nil {
		return nil, fmt.Errorf("responder handshake write: %w", err)
	}
	if err := writeFrame(conn, reply); err != nil {
		return nil, fmt.Errorf("send handshake reply: %w", err)
	}
	if cs1 == nil || cs2 == nil {
		return nil, fmt.Errorf("responder handshake did not complete")
	}
	if err := conn.SetDeadline(time.Time{}); err != nil {
		return nil, fmt.Errorf("clear handshake deadline: %w", err)
	}
	// Same direction convention as the initiator: cs1 initiator -> responder.
	return &Session{conn: conn, send: cs2, recv: cs1}, nil
}

// Write seals p into one or more AEAD frames and writes them. Chunks larger
// than MaxPayloadPerFrame are split. Each frame's tag covers its length, so
// truncation or splicing is detected.
func (s *Session) Write(p []byte) (int, error) {
	written := 0
	for len(p) > 0 {
		chunk := p
		if len(chunk) > MaxPayloadPerFrame {
			chunk = chunk[:MaxPayloadPerFrame]
		}
		ciphertext, err := s.send.Encrypt(nil, nil, chunk)
		if err != nil {
			return written, fmt.Errorf("encrypt frame: %w", err)
		}
		if err := writeFrame(s.conn, ciphertext); err != nil {
			return written, fmt.Errorf("send frame: %w", err)
		}
		written += len(chunk)
		p = p[len(chunk):]
	}
	return written, nil
}

// Read opens the next frame into p, buffering any remainder of the current
// frame for subsequent calls. It returns io.EOF when the peer closed the
// connection cleanly between frames.
func (s *Session) Read(p []byte) (int, error) {
	if len(s.buf) == 0 {
		frame, err := readFrame(s.conn)
		if err != nil {
			return 0, err
		}
		plaintext, err := s.recv.Decrypt(nil, nil, frame)
		if err != nil {
			return 0, fmt.Errorf("decrypt frame: %w", err)
		}
		s.buf = plaintext
	}
	n := copy(p, s.buf)
	s.buf = s.buf[n:]
	return n, nil
}

func (s *Session) Close() error                       { return s.conn.Close() }
func (s *Session) LocalAddr() net.Addr                { return s.conn.LocalAddr() }
func (s *Session) RemoteAddr() net.Addr               { return s.conn.RemoteAddr() }
func (s *Session) SetDeadline(t time.Time) error      { return s.conn.SetDeadline(t) }
func (s *Session) SetReadDeadline(t time.Time) error  { return s.conn.SetReadDeadline(t) }
func (s *Session) SetWriteDeadline(t time.Time) error { return s.conn.SetWriteDeadline(t) }

func writeFrame(w io.Writer, payload []byte) error {
	if len(payload) > maxFrameSize {
		return fmt.Errorf("frame of %d bytes exceeds %d", len(payload), maxFrameSize)
	}
	var header [frameHeaderSize]byte
	binary.BigEndian.PutUint16(header[:], uint16(len(payload)))
	if _, err := w.Write(header[:]); err != nil {
		return err
	}
	_, err := w.Write(payload)
	return err
}

func readFrame(r io.Reader) ([]byte, error) {
	var header [frameHeaderSize]byte
	if _, err := io.ReadFull(r, header[:]); err != nil {
		return nil, err
	}
	length := binary.BigEndian.Uint16(header[:])
	if length == 0 {
		return nil, fmt.Errorf("empty frame")
	}
	payload := make([]byte, length)
	if _, err := io.ReadFull(r, payload); err != nil {
		return nil, err
	}
	return payload, nil
}
