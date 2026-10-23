package proxy

import (
	"bufio"
	"io"
	"net"
	"net/http"
	"net/http/httptest"
	"reflect"
	"strconv"
	"strings"
	"sync/atomic"
	"testing"
	"time"

	"llm-gateway/internal/config"
	"llm-gateway/internal/keymap"
)

func gateway(t *testing.T, target config.Target) *httptest.Server {
	t.Helper()
	p := New(nil)
	s := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		p.ServeHTTP(w, r.WithContext(keymap.WithTarget(r.Context(), target)))
	}))
	t.Cleanup(s.Close)
	return s
}

func TestTransparentForwarding(t *testing.T) {
	for _, xff := range []string{"", "203.0.113.1"} {
		t.Run("xff="+xff, func(t *testing.T) {
			body := "\x00raw body\nnot JSON"
			var expectedHost string
			upstream := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
				data, _ := io.ReadAll(r.Body)
				if r.RequestURI != "/arbitrary/a%2Fb?x=1&x=2&raw=%2f" || r.Method != "PATCH" || string(data) != body {
					t.Errorf("request changed: %s %s %q", r.Method, r.RequestURI, data)
				}
				want := http.Header{"Authorization": {"Bearer real"}, "X-Custom": {"one", "two"}, "Content-Type": {"application/octet-stream"}, "User-Agent": {"original"}, "Content-Length": {strconv.Itoa(len(body))}}
				if xff != "" {
					want.Set("X-Forwarded-For", xff)
				}
				if !reflect.DeepEqual(r.Header, want) {
					t.Errorf("headers = %#v, want %#v", r.Header, want)
				}
				if r.Host != expectedHost {
					t.Errorf("upstream Host = %q, want target authority %q", r.Host, expectedHost)
				}
				w.Header().Set("X-Upstream", "unchanged")
				w.WriteHeader(http.StatusTooManyRequests)
				_, _ = w.Write([]byte("original error"))
			}))
			defer upstream.Close()
			expectedHost = upstream.Listener.Addr().String()
			s := gateway(t, config.Target{BaseURL: upstream.URL, APIKey: "real"})
			req, _ := http.NewRequest("PATCH", s.URL+"/arbitrary/a%2Fb?x=1&x=2&raw=%2f", strings.NewReader(body))
			req.Host = "original.example"
			req.Header.Set("Authorization", "Bearer virtual")
			req.Header["X-Custom"] = []string{"one", "two"}
			req.Header.Set("Content-Type", "application/octet-stream")
			req.Header.Set("User-Agent", "original")
			if xff != "" {
				req.Header.Set("X-Forwarded-For", xff)
			}
			client := &http.Client{Transport: &http.Transport{DisableCompression: true}, Timeout: 5 * time.Second}
			defer client.CloseIdleConnections()
			resp, err := client.Do(req)
			if err != nil {
				t.Fatal(err)
			}
			defer resp.Body.Close()
			data, _ := io.ReadAll(resp.Body)
			if resp.StatusCode != 429 || string(data) != "original error" || resp.Header.Get("X-Upstream") != "unchanged" {
				t.Fatalf("response changed: %d %q %v", resp.StatusCode, data, resp.Header)
			}
		})
	}
}

func TestHopHeadersAndOptionalKey(t *testing.T) {
	upstream := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		for _, h := range []string{"Authorization", "Connection", "X-Hop"} {
			if r.Header.Get(h) != "" {
				t.Errorf("unexpected %s", h)
			}
		}
		w.WriteHeader(204)
	}))
	defer upstream.Close()
	s := gateway(t, config.Target{BaseURL: upstream.URL})
	req, _ := http.NewRequest("GET", s.URL, nil)
	req.Header.Set("Authorization", "Bearer virtual")
	req.Header.Set("Connection", "X-Hop")
	req.Header.Set("X-Hop", "remove-me")
	resp, err := http.DefaultClient.Do(req)
	if err != nil {
		t.Fatal(err)
	}
	resp.Body.Close()
	if resp.StatusCode != 204 {
		t.Fatal(resp.Status)
	}
}

func TestUnavailable(t *testing.T) {
	listener, err := net.Listen("tcp", "127.0.0.1:0")
	if err != nil {
		t.Fatal(err)
	}
	address := listener.Addr().String()
	listener.Close()
	s := gateway(t, config.Target{BaseURL: "http://" + address})
	resp, err := http.Get(s.URL)
	if err != nil {
		t.Fatal(err)
	}
	defer resp.Body.Close()
	data, _ := io.ReadAll(resp.Body)
	if resp.StatusCode != 502 || string(data) != "Bad Gateway\n" {
		t.Fatal(resp.Status, string(data))
	}
}

func TestSSEFlush(t *testing.T) {
	release := make(chan struct{})
	upstream := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		if r.RequestURI != "/v1/stream" {
			t.Errorf("SSE path = %q", r.RequestURI)
		}
		w.Header().Set("Content-Type", "text/event-stream")
		_, _ = io.WriteString(w, "data: first\n\n")
		w.(http.Flusher).Flush()
		select {
		case <-release:
		case <-r.Context().Done():
			return
		}
		_, _ = io.WriteString(w, "data: second\n\n")
	}))
	defer upstream.Close()
	s := gateway(t, config.Target{BaseURL: upstream.URL, Rewrite: []config.RewriteRule{{Source: "/api", Destination: "/v1"}}})
	defer close(release)
	client := &http.Client{Timeout: 5 * time.Second}
	resp, err := client.Get(s.URL + "/api/stream")
	if err != nil {
		t.Fatal(err)
	}
	defer resp.Body.Close()
	line, err := bufio.NewReader(resp.Body).ReadString('\n')
	if err != nil || line != "data: first\n" {
		t.Fatalf("first chunk did not arrive before upstream completion: %q %v", line, err)
	}
}

func TestNoRetry(t *testing.T) {
	var attempts atomic.Int64
	upstream := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		attempts.Add(1)
		conn, _, err := w.(http.Hijacker).Hijack()
		if err != nil {
			t.Error(err)
			return
		}
		conn.Close()
	}))
	defer upstream.Close()
	s := gateway(t, config.Target{BaseURL: upstream.URL})
	resp, err := http.Post(s.URL, "text/plain", strings.NewReader("one attempt"))
	if err != nil {
		t.Fatal(err)
	}
	resp.Body.Close()
	if resp.StatusCode != 502 || attempts.Load() != 1 {
		t.Fatal(resp.Status, attempts.Load())
	}
}

func TestPeerForwardingOverE2EE(t *testing.T) {
	// Upstream lives behind the peer gateway's e2ee listener: the peer
	// decrypts the Noise stream and serves plain HTTP from it.
	var gotAuth, gotPath string
	upstream := http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		gotAuth, gotPath = r.Header.Get("Authorization"), r.URL.RequestURI()
		w.WriteHeader(http.StatusTeapot)
		_, _ = w.Write([]byte("behind peer"))
	})
	clientID, peerID, err := e2eeIdentities()
	if err != nil {
		t.Fatal(err)
	}
	peerAddr, stop := startE2EEServer(t, peerID, map[string][]byte{"client": clientID.Public.Public}, upstream)
	defer stop()
	dialer := e2eeDialer(clientID, peerAddr, peerID.Public.Public)
	s := gatewayWithPeers(t, config.Target{BaseURL: "http://" + peerAddr, APIKey: "sk-to-peer", Peer: "peer"},
		map[string]PeerDialer{peerAddr: dialer})
	resp, err := http.Post(s.URL+"/v1/chat?q=1", "application/json", strings.NewReader("{}"))
	if err != nil {
		t.Fatal(err)
	}
	defer resp.Body.Close()
	body, _ := io.ReadAll(resp.Body)
	if resp.StatusCode != http.StatusTeapot || string(body) != "behind peer" {
		t.Fatalf("status=%d body=%q", resp.StatusCode, body)
	}
	if gotAuth != "Bearer sk-to-peer" || gotPath != "/v1/chat?q=1" {
		t.Fatalf("upstream saw auth=%q path=%q", gotAuth, gotPath)
	}
}

// SSE through the encrypted hop must keep chunk-by-chunk flushing: the client
// receives the first event while the upstream is still streaming, proving the
// Noise session does not buffer the whole response.
func TestSSEStreamingOverE2EE(t *testing.T) {
	release := make(chan struct{})
	upstream := http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		if r.RequestURI != "/v1/stream" {
			t.Errorf("encrypted SSE path = %q", r.RequestURI)
		}
		w.Header().Set("Content-Type", "text/event-stream")
		_, _ = io.WriteString(w, "data: first\n\n")
		w.(http.Flusher).Flush()
		select {
		case <-release:
		case <-r.Context().Done():
			return
		}
		_, _ = io.WriteString(w, "data: second\n\n")
	})
	defer close(release)
	clientID, peerID, err := e2eeIdentities()
	if err != nil {
		t.Fatal(err)
	}
	peerAddr, stop := startE2EEServer(t, peerID, map[string][]byte{"client": clientID.Public.Public}, upstream)
	defer stop()
	dialer := e2eeDialer(clientID, peerAddr, peerID.Public.Public)
	s := gatewayWithPeers(t, config.Target{BaseURL: "http://" + peerAddr, APIKey: "sk-virt", Peer: "peer", Rewrite: []config.RewriteRule{{Source: "/api", Destination: "/v1"}}},
		map[string]PeerDialer{peerAddr: dialer})
	resp, err := http.Get(s.URL + "/api/stream")
	if err != nil {
		t.Fatal(err)
	}
	defer resp.Body.Close()
	reader := bufio.NewReader(resp.Body)
	line, err := reader.ReadString('\n')
	if err != nil || line != "data: first\n" {
		t.Fatalf("first chunk did not arrive before upstream completion: %q %v", line, err)
	}
	// The upstream cannot finish until this point; receiving the first event
	// proves the encrypted hop flushed it immediately.
	release <- struct{}{}
	rest, err := io.ReadAll(reader)
	if err != nil || !strings.Contains(string(rest), "data: second\n") {
		t.Fatalf("second chunk: %q %v", rest, err)
	}
}
