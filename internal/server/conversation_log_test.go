package server

import (
	"bytes"
	"context"
	"encoding/base64"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"log/slog"
	"net"
	"net/http"
	"net/http/httptest"
	"os"
	"path/filepath"
	"strings"
	"sync"
	"testing"
	"time"

	"llm-gateway/internal/config"
	"llm-gateway/internal/conversationlog"
	"llm-gateway/internal/keymap"
)

func quietLogger() *slog.Logger { return slog.New(slog.NewJSONHandler(io.Discard, nil)) }
func conversationServer(t *testing.T, entry config.Entry, limit int) (*http.Server, string) {
	t.Helper()
	dir := t.TempDir()
	store := conversationlog.New(dir, limit, quietLogger())
	return NewWithRecorder("", keymap.New(map[string][]config.Entry{"virtual": {entry}}), quietLogger(), nil, store), dir
}
func readConversations(t *testing.T, dir string, n int) []conversationlog.Record {
	t.Helper()
	deadline := time.Now().Add(5 * time.Second)
	for {
		files, err := filepath.Glob(filepath.Join(dir, "*", "*.json"))
		if err != nil {
			t.Fatal(err)
		}
		if len(files) == n {
			records := make([]conversationlog.Record, 0, n)
			for _, f := range files {
				b, err := os.ReadFile(f)
				if err != nil {
					t.Fatal(err)
				}
				var r conversationlog.Record
				if err = json.Unmarshal(b, &r); err != nil {
					t.Fatal(err)
				}
				records = append(records, r)
			}
			return records
		}
		if time.Now().After(deadline) {
			t.Fatalf("wanted %d records, got %d", n, len(files))
		}
		time.Sleep(5 * time.Millisecond)
	}
}
func conversationRequest(body string) *http.Request {
	r := httptest.NewRequest("POST", "/api/chat?secret-query=hidden", strings.NewReader(body))
	r.Header.Set("Authorization", "Bearer virtual")
	return r
}

func TestConversationAuthAliasesAndFailures(t *testing.T) {
	up := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		b, _ := io.ReadAll(r.Body)
		if r.URL.EscapedPath() != "/v1/chat" || r.URL.RawQuery != "secret-query=hidden" || string(b) != `{"model":"real-model","messages":[]}` {
			t.Errorf("forwarded %s %s", r.URL, string(b))
		}
		if r.Header.Get("Authorization") != "Bearer secret-key" || r.Header.Get("X-Forwarded-For") != "" || r.Header.Get("X-Request-ID") != "" {
			t.Errorf("headers: %v", r.Header)
		}
		w.Header().Set("X-Reply", "unchanged")
		w.WriteHeader(429)
		_, _ = io.WriteString(w, `{"error":"quota"}`)
	}))
	defer up.Close()
	entry := config.Entry{Target: config.Target{BaseURL: up.URL, APIKey: "secret-key", Rewrite: []config.RewriteRule{{Source: "/api", Destination: "/v1"}}}, Models: []config.Model{{ID: "alias", Name: "real-model"}}}
	srv, dir := conversationServer(t, entry, 1024)
	unauthorized := conversationRequest("do not capture")
	unauthorized.Header.Del("Authorization")
	srv.Handler.ServeHTTP(httptest.NewRecorder(), unauthorized)
	readConversations(t, dir, 0)
	body := `{"model":"alias","messages":[]}`
	w := httptest.NewRecorder()
	srv.Handler.ServeHTTP(w, conversationRequest(body))
	r := readConversations(t, dir, 1)[0]
	if w.Code != 429 || w.Header().Get("X-Reply") != "unchanged" || w.Body.String() != `{"error":"quota"}` {
		t.Fatal(w)
	}
	if r.Request.Data != body || r.Response.Data != w.Body.String() || r.Status != 429 || !r.ResponseComplete || r.Path != "/api/chat" || r.ModelID != "alias" || r.ModelName != "real-model" || !r.Request.Complete {
		t.Fatalf("%+v", r)
	}
	serialized, _ := json.Marshal(r)
	for _, secret := range []string{"secret-key", "secret-query", "hidden", "Bearer"} {
		if bytes.Contains(serialized, []byte(secret)) {
			t.Fatalf("leaked %s", secret)
		}
	}
	w = httptest.NewRecorder()
	srv.Handler.ServeHTTP(w, conversationRequest(`{"model":"unknown"}`))
	records := readConversations(t, dir, 2)
	found := false
	for _, r := range records {
		if r.Status == 400 {
			found = true
			if r.Error != "preparation_failed" || !r.ResponseComplete {
				t.Fatal(r)
			}
		}
	}
	if !found {
		t.Fatal("no preparation rejection")
	}
}

func TestConversationDisabledAndUnavailable(t *testing.T) {
	up := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) { _, _ = io.Copy(w, r.Body) }))
	defer up.Close()
	keys := keymap.New(map[string][]config.Entry{"virtual": {{Target: config.Target{BaseURL: up.URL}}}})
	for _, enabled := range []bool{false, true} {
		dir := filepath.Join(t.TempDir(), "blocked")
		if err := os.WriteFile(dir, []byte("x"), 0600); err != nil {
			t.Fatal(err)
		}
		var store *conversationlog.Store
		if enabled {
			store = conversationlog.New(dir, 8, quietLogger())
		}
		srv := NewWithRecorder("", keys, quietLogger(), nil, store)
		w := httptest.NewRecorder()
		srv.Handler.ServeHTTP(w, conversationRequest("unchanged"))
		if w.Code != 200 || w.Body.String() != "unchanged" {
			t.Fatal(w)
		}
	}
}

func TestConversationBinaryLimitsAndConcurrentCalls(t *testing.T) {
	up := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		b, _ := io.ReadAll(r.Body)
		w.WriteHeader(201)
		_, _ = w.Write(b)
	}))
	defer up.Close()
	for _, body := range []string{string([]byte{0xff, 0xfe, 0x00, 0x01}), "你好abcdef"} {
		srv, dir := conversationServer(t, config.Entry{Target: config.Target{BaseURL: up.URL}}, 2)
		var wg sync.WaitGroup
		for i := 0; i < 10; i++ {
			wg.Add(1)
			go func() {
				defer wg.Done()
				w := httptest.NewRecorder()
				req := conversationRequest(body)
				req.Header.Set("Content-Encoding", "gzip")
				srv.Handler.ServeHTTP(w, req)
				if w.Code != 201 || w.Body.String() != body {
					t.Error("forwarding changed")
				}
			}()
		}
		wg.Wait()
		records := readConversations(t, dir, 10)
		seen := map[string]bool{}
		for _, r := range records {
			if seen[r.RequestID] {
				t.Fatal("duplicate id")
			}
			seen[r.RequestID] = true
			for _, b := range []conversationlog.Body{r.Request, r.Response} {
				if !b.Truncated || !b.Complete || b.CapturedBytes != 2 || b.ObservedBytes != int64(len(body)) || b.Encoding != "base64" {
					t.Fatalf("%+v", b)
				}
				data, err := base64.StdEncoding.DecodeString(b.Data)
				if err != nil || !bytes.Equal(data, []byte(body)[:2]) {
					t.Fatal("binary not lossless")
				}
			}
		}
	}
}

func TestConversationSSEFlushAndCancellation(t *testing.T) {
	for _, cancelCall := range []bool{false, true} {
		t.Run(fmt.Sprint(cancelCall), func(t *testing.T) {
			release := make(chan struct{})
			up := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
				w.Header().Set("Content-Type", "text/event-stream")
				_, _ = io.WriteString(w, "data: first\n\n")
				w.(http.Flusher).Flush()
				select {
				case <-release:
					_, _ = io.WriteString(w, "data: last\n\n")
				case <-r.Context().Done():
				}
			}))
			defer up.Close()
			srv, dir := conversationServer(t, config.Entry{Target: config.Target{BaseURL: up.URL}}, 1024)
			gateway := httptest.NewServer(srv.Handler)
			defer gateway.Close()
			ctx, cancel := context.WithTimeout(context.Background(), 5*time.Second)
			defer cancel()
			req, _ := http.NewRequestWithContext(ctx, "POST", gateway.URL+"/v1/chat", strings.NewReader(`{"stream":true}`))
			req.Header.Set("Authorization", "Bearer virtual")
			resp, err := http.DefaultClient.Do(req)
			if err != nil {
				t.Fatal(err)
			}
			first := make([]byte, len("data: first\n\n"))
			if _, err := io.ReadFull(resp.Body, first); err != nil {
				t.Fatal(err)
			}
			if string(first) != "data: first\n\n" {
				t.Fatal(string(first))
			}
			if cancelCall {
				cancel()
				_ = resp.Body.Close()
			} else {
				close(release)
				rest, err := io.ReadAll(resp.Body)
				_ = resp.Body.Close()
				if err != nil || string(rest) != "data: last\n\n" {
					t.Fatalf("%q %v", rest, err)
				}
			}
			r := readConversations(t, dir, 1)[0]
			if r.Status != 200 || !strings.HasPrefix(r.Response.Data, "data: first\n\n") {
				t.Fatal(r)
			}
			if cancelCall {
				if r.ResponseComplete || r.Error == "" {
					t.Fatal(r)
				}
			} else if !r.ResponseComplete || r.Response.Data != "data: first\n\ndata: last\n\n" {
				t.Fatal(r)
			}
		})
	}
}

func TestConversationTruncatedUpstreamAndConnectionRefused(t *testing.T) {
	up := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		w.Header().Set("Content-Length", "100")
		_, _ = io.WriteString(w, "partial")
	}))
	defer up.Close()
	srv, dir := conversationServer(t, config.Entry{Target: config.Target{BaseURL: up.URL}}, 1024)
	gateway := httptest.NewServer(srv.Handler)
	defer gateway.Close()
	req, _ := http.NewRequest("POST", gateway.URL+"/chat", nil)
	req.Header.Set("Authorization", "Bearer virtual")
	resp, err := http.DefaultClient.Do(req)
	if err == nil {
		_, _ = io.ReadAll(resp.Body)
		_ = resp.Body.Close()
	}
	r := readConversations(t, dir, 1)[0]
	if r.ResponseComplete || r.Error == "" {
		t.Fatal(r)
	}
	ln, err := net.Listen("tcp", "127.0.0.1:0")
	if err != nil {
		t.Fatal(err)
	}
	addr := ln.Addr().String()
	_ = ln.Close()
	srv, dir = conversationServer(t, config.Entry{Target: config.Target{BaseURL: "http://" + addr}}, 1024)
	w := httptest.NewRecorder()
	unread := conversationRequest("body")
	unread.Header.Set("Content-Encoding", "gzip")
	srv.Handler.ServeHTTP(w, unread)
	r = readConversations(t, dir, 1)[0]
	if w.Code != 502 || r.Status != 502 || r.Error != "upstream_transport" || !r.ResponseComplete || r.Request.Complete {
		t.Fatalf("%+v", r)
	}
}

type partialWriter struct{ header http.Header }

func (w *partialWriter) Header() http.Header         { return w.header }
func (w *partialWriter) WriteHeader(int)             {}
func (w *partialWriter) Write(p []byte) (int, error) { return min(2, len(p)), io.ErrClosedPipe }
func TestConversationWriterPartialAndHijack(t *testing.T) {
	store := conversationlog.New(t.TempDir(), 8, quietLogger())
	call, err := store.Begin("POST", "/", 0)
	if err != nil {
		t.Fatal(err)
	}
	w := &responseWriter{ResponseWriter: &partialWriter{header: make(http.Header)}, call: call}
	n, err := w.Write([]byte("hello"))
	if n != 2 || !errors.Is(err, io.ErrClosedPipe) {
		t.Fatal(n, err)
	}
	r := call.Finish(time.Now(), 200, "", "", "")
	if r.Response.Data != "he" || r.ResponseComplete || r.Error != "client_write" {
		t.Fatal(r)
	}
}
