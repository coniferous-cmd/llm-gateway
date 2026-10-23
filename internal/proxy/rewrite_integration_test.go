package proxy

import (
	"io"
	"net/http"
	"net/http/httptest"
	"reflect"
	"strings"
	"testing"

	"llm-gateway/internal/config"
	"llm-gateway/internal/keymap"
)

func TestRewriteUpstreamRequestURI(t *testing.T) {
	rules := []config.RewriteRule{{Source: "/api", Destination: "/v1"}, {Source: "/openai", Destination: "/v1"}, {Source: "/v1", Destination: "/v2"}}
	remove := []config.RewriteRule{{Source: "/api", Destination: ""}}
	for _, tt := range []struct {
		name, path, want string
		rules            []config.RewriteRule
	}{
		{"delete exact", "/api", "/", remove},
		{"delete empty query", "/api?", "/?", remove},
		{"delete trailing slash", "/api/", "/", remove},
		{"delete encoded suffix", "/api/a%2Fb%20c/%25/%2f?next=/api&x=a%2Fb&x=2", "/a%2Fb%20c/%25/%2f?next=/api&x=a%2Fb&x=2", remove},
		{"delete boundary", "/apix/models", "/apix/models", remove},
		{"delete encoded boundary", "/api%2Fmodels", "/api%2Fmodels", remove},
		{"delete trailing source", "/api/models", "/models", []config.RewriteRule{{Source: "/api/", Destination: ""}}},
		{"rewritten", "/api/chat?next=/api&x=a%2Fb&x=2", "/v1/chat?next=/api&x=a%2Fb&x=2", rules},
		{"second rule", "/openai/models", "/v1/models", rules},
		{"escaped suffix", "/api/files/a%2Fb%20c/%25/%2f", "/v1/files/a%2Fb%20c/%25/%2f", rules},
		{"encoded boundary", "/api%2Fmodels?raw=%2f", "/api%2Fmodels?raw=%2f", rules},
		{"encoded prefix", "/%61pi/models", "/%61pi/models", rules},
		{"empty query", "/api/models?", "/v1/models?", rules},
		{"no query", "/api/models", "/v1/models", rules},
		{"unmatched", "/apix/a%2Fb?next=/api", "/apix/a%2Fb?next=/api", rules},
		{"no rules", "/api/a%2Fb?next=/api", "/api/a%2Fb?next=/api", nil},
		{"encoded destination", "/api/models", "/v%20x/models", []config.RewriteRule{{Source: "/api", Destination: "/v%20x"}}},
	} {
		t.Run(tt.name, func(t *testing.T) {
			const payload = "\x00not JSON\n"
			var expectedHost string
			upstream := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
				body, _ := io.ReadAll(r.Body)
				if r.RequestURI != tt.want || r.Method != "PATCH" || string(body) != payload {
					t.Errorf("upstream request = %s %s %q, want path %s", r.Method, r.RequestURI, body, tt.want)
				}
				if r.Host != expectedHost || r.Header.Get("Authorization") != "Bearer real" || r.Header.Get("X-Custom") != "original" || r.Header.Get("X-Forwarded-For") != "203.0.113.1" {
					t.Errorf("Host=%s headers=%v", r.Host, r.Header)
				}
				w.Header().Set("X-Upstream", "unchanged")
				w.WriteHeader(http.StatusTooManyRequests)
				_, _ = io.WriteString(w, "original error")
			}))
			defer upstream.Close()
			expectedHost = upstream.Listener.Addr().String()
			target := config.Target{BaseURL: upstream.URL, APIKey: "real", Rewrite: tt.rules}
			r := httptest.NewRequest("PATCH", "http://gateway.example"+tt.path, strings.NewReader(payload))
			r.Header.Set("Authorization", "Bearer virtual")
			r.Header.Set("X-Custom", "original")
			r.Header.Set("X-Forwarded-For", "203.0.113.1")
			r = r.WithContext(keymap.WithTarget(r.Context(), target))
			before := *r.URL
			headers := r.Header.Clone()
			w := httptest.NewRecorder()
			New(nil).ServeHTTP(w, r)
			if w.Code != http.StatusTooManyRequests || w.Body.String() != "original error" || w.Header().Get("X-Upstream") != "unchanged" {
				t.Fatalf("response = %d %q %v", w.Code, w.Body.String(), w.Header())
			}
			if !reflect.DeepEqual(*r.URL, before) || !reflect.DeepEqual(r.Header, headers) || r.Host != "gateway.example" {
				t.Fatalf("original request changed: %+v", r)
			}
		})
	}
}

func TestPrepareAndRewriteSelection(t *testing.T) {
	upstream := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		body, _ := io.ReadAll(r.Body)
		_, _ = io.WriteString(w, r.RequestURI+"|"+r.Header.Get("Authorization")+"|"+string(body))
	}))
	defer upstream.Close()
	// Parsing real configuration exercises field placement and the complete
	// config -> keymap -> Prepare -> context -> Director chain.
	cfg, err := config.Parse([]byte("keys:\n" +
		"  - token: dev\n    target:\n      base_url: " + upstream.URL + "\n      api_key: one\n      rewrite: ['/api:/v1']\n    models: [{id: one, name: real-one}]\n" +
		"  - token: dev\n    target:\n      base_url: " + upstream.URL + "\n      api_key: two\n      rewrite: ['/api:-']\n    models: [{id: two, name: real-two}]\n"))
	if err != nil {
		t.Fatal(err)
	}
	route, _ := keymap.New(cfg.Keys).Select("Bearer dev")
	p := New(nil)
	for _, tt := range []struct{ body, want string }{
		{`{ "model": "two", "x": 1 }`, `/chat?next=/api|Bearer two|{ "model": "real-two", "x": 1 }`},
		{"", "/v1/chat?next=/api|Bearer one|"},
		{`{"model":"one"}`, `/v1/chat?next=/api|Bearer one|{"model":"real-one"}`},
		{"", "/chat?next=/api|Bearer two|"},
		{"", "/v1/chat?next=/api|Bearer one|"},
	} {
		r := httptest.NewRequest("POST", "http://gateway.example/api/chat?next=/api", strings.NewReader(tt.body))
		target, _, err := Prepare(r, route)
		if err != nil {
			t.Fatal(err)
		}
		w := httptest.NewRecorder()
		p.ServeHTTP(w, r.WithContext(keymap.WithTarget(r.Context(), target)))
		if w.Code != http.StatusOK || w.Body.String() != tt.want {
			t.Fatalf("body %q -> status %d, body %q, want %q", tt.body, w.Code, w.Body.String(), tt.want)
		}
	}
}
