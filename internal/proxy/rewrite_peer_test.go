package proxy

import (
	"io"
	"net/http"
	"strings"
	"testing"

	"llm-gateway/internal/config"
)

func TestPeerPathRewrite(t *testing.T) {
	clientID, peerID, err := e2eeIdentities()
	if err != nil {
		t.Fatal(err)
	}
	upstream := http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		body, _ := io.ReadAll(r.Body)
		if r.Method != "POST" || r.Header.Get("Authorization") != "Bearer peer-key" || string(body) != `{"model":"unchanged"}` {
			t.Errorf("encrypted request changed: %s %v %q", r.Method, r.Header, body)
		}
		w.Header().Set("X-Peer", "unchanged")
		w.WriteHeader(http.StatusTeapot)
		_, _ = io.WriteString(w, r.RequestURI)
	})
	peerAddr, stop := startE2EEServer(t, peerID, map[string][]byte{"client": clientID.Public.Public}, upstream)
	defer stop()
	target := config.Target{
		BaseURL: "http://" + peerAddr, APIKey: "peer-key", Peer: "peer",
		Rewrite: []config.RewriteRule{{Source: "/strip", Destination: ""}, {Source: "/api", Destination: "/v1"}, {Source: "/v1", Destination: "/v2"}},
	}
	s := gatewayWithPeers(t, target, map[string]PeerDialer{peerAddr: e2eeDialer(clientID, peerAddr, peerID.Public.Public)})
	for _, tt := range []struct{ path, want string }{
		{"/api/a%2Fb%20c/%25/%2f?next=/api&x=a%2Fb&x=2", "/v1/a%2Fb%20c/%25/%2f?next=/api&x=a%2Fb&x=2"},
		{"/strip/api/a%2Fb%20c/%25/%2f?next=/api&x=a%2Fb&x=2", "/api/a%2Fb%20c/%25/%2f?next=/api&x=a%2Fb&x=2"},
		{"/strip?", "/?"},
	} {
		resp, err := s.Client().Post(s.URL+tt.path, "application/json", strings.NewReader(`{"model":"unchanged"}`))
		if err != nil {
			t.Fatal(err)
		}
		body, err := io.ReadAll(resp.Body)
		resp.Body.Close()
		if err != nil {
			t.Fatal(err)
		}
		if resp.StatusCode != http.StatusTeapot || resp.Header.Get("X-Peer") != "unchanged" || string(body) != tt.want {
			t.Fatalf("peer response for %s = %d %v %q, want %q", tt.path, resp.StatusCode, resp.Header, body, tt.want)
		}
	}
}
