package server

import (
	"encoding/json"
	"io"
	"net/http"
	"net/http/httptest"
	"reflect"
	"strings"
	"testing"

	"llm-gateway/internal/config"
	"llm-gateway/internal/conversationlog"
	"llm-gateway/internal/keymap"
)

func TestConversationForwardingParity(t *testing.T) {
	type observed struct {
		Method, URI, Body string
		Header            http.Header
	}
	seen := make(chan observed, 2)
	upstream := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		body, _ := io.ReadAll(r.Body)
		seen <- observed{r.Method, r.URL.RequestURI(), string(body), r.Header.Clone()}
		w.Header().Set("Content-Type", "application/json")
		w.Header().Set("X-Custom-Reply", "same")
		w.WriteHeader(429)
		_, _ = io.WriteString(w, `{"error":"same"}`)
	}))
	defer upstream.Close()
	entry := config.Entry{Target: config.Target{BaseURL: upstream.URL, APIKey: "upstream-key", Rewrite: []config.RewriteRule{{Source: "/api", Destination: "/v1"}}}, Models: []config.Model{{ID: "alias", Name: "real"}}}
	var requests []observed
	var replies []*httptest.ResponseRecorder
	for _, enabled := range []bool{false, true} {
		keys := keymap.New(map[string][]config.Entry{"virtual": {entry}})
		var recorder *conversationlog.Store
		if enabled {
			recorder = conversationlog.New(t.TempDir(), 8, quietLogger())
		}
		srv := NewWithRecorder("", keys, quietLogger(), nil, recorder)
		req := conversationRequest(`{"model":"alias","messages":[{"role":"user","content":"hello"}]}`)
		req.Header.Set("Content-Type", "application/json")
		req.Header.Set("X-Custom", "original")
		req.Header.Set("X-Forwarded-For", "existing")
		w := httptest.NewRecorder()
		srv.Handler.ServeHTTP(w, req)
		requests = append(requests, <-seen)
		replies = append(replies, w)
	}
	if !reflect.DeepEqual(requests[0], requests[1]) {
		t.Fatalf("requests differ: %+v / %+v", requests[0], requests[1])
	}
	if replies[0].Code != replies[1].Code || replies[0].Body.String() != replies[1].Body.String() || !reflect.DeepEqual(replies[0].Header(), replies[1].Header()) {
		t.Fatal("response changed")
	}
}

func TestConversationPreparation413Bounded(t *testing.T) {
	srv, dir := conversationServer(t, config.Entry{Target: config.Target{BaseURL: "http://127.0.0.1:1"}, Models: []config.Model{{ID: "alias", Name: "alias"}}}, 8)
	w := httptest.NewRecorder()
	srv.Handler.ServeHTTP(w, conversationRequest(strings.Repeat("x", (32<<20)+1)))
	r := readConversations(t, dir, 1)[0]
	if w.Code != 413 || r.Status != 413 || !r.Request.Truncated || r.Request.CapturedBytes != 8 || r.Error != "preparation_failed" {
		t.Fatal(r)
	}
	b, err := json.Marshal(r)
	if err != nil || len(b) > 4096 {
		t.Fatalf("record not bounded: %d %v", len(b), err)
	}
}
