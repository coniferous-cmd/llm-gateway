package server

import (
	"bufio"
	"bytes"
	"context"
	"encoding/json"
	"io"
	"log/slog"
	"net"
	"net/http"
	"net/http/httptest"
	"strings"
	"sync/atomic"
	"testing"
	"time"

	"llm-gateway/internal/config"
	"llm-gateway/internal/keymap"
)

func TestAuthenticationRoutingAndLogs(t *testing.T) {
	var calls atomic.Int64
	upstream := func(name string) *httptest.Server {
		return httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
			calls.Add(1)
			if r.Header.Get("Authorization") != "Bearer real-"+name {
				t.Errorf("wrong upstream credentials: %q", r.Header.Get("Authorization"))
			}
			if r.Method != "PATCH" || r.RequestURI != "/custom/a%2Fb?raw=%2f" {
				t.Errorf("request changed: %s %s", r.Method, r.RequestURI)
			}
			w.Header().Set("X-Target", name)
			w.WriteHeader(http.StatusAccepted)
			_, _ = io.Copy(w, r.Body)
		}))
	}
	first, second := upstream("first"), upstream("second")
	defer first.Close()
	defer second.Close()
	var logs bytes.Buffer
	keys := keymap.New(map[string][]config.Target{"virtual": {
		{BaseURL: first.URL, APIKey: "real-first"},
		{BaseURL: second.URL, APIKey: "real-second"},
	}})
	srv := New(config.DefaultListen, keys, slog.New(slog.NewJSONHandler(&logs, nil)))
	if srv.WriteTimeout != 0 || srv.ReadTimeout != 0 || srv.Addr != config.DefaultListen {
		t.Fatal("streaming timeout or default address mismatch")
	}
	requests := 0
	send := func(auth string) *httptest.ResponseRecorder {
		t.Helper()
		requests++
		req := httptest.NewRequest("PATCH", "/custom/a%2Fb?raw=%2f", strings.NewReader("opaque payload"))
		req.Header.Set("Authorization", auth)
		w := httptest.NewRecorder()
		srv.Handler.ServeHTTP(w, req)
		return w
	}
	for _, auth := range []string{"", "Basic virtual", "Bearer unknown", "Bearer virtual extra"} {
		if w := send(auth); w.Code != 401 {
			t.Fatalf("accepted %q: %d", auth, w.Code)
		}
	}
	if calls.Load() != 0 {
		t.Fatal("unauthorized request reached upstream")
	}
	for _, name := range []string{"first", "second", "first", "second"} {
		w := send("Bearer virtual")
		if w.Code != 202 || w.Body.String() != "opaque payload" || w.Header().Get("X-Target") != name {
			t.Fatalf("incorrect routing: %d %q %v", w.Code, w.Body, w.Header())
		}
	}
	lines := strings.Split(strings.TrimSpace(logs.String()), "\n")
	if len(lines) != requests {
		t.Fatalf("got %d logs for %d requests", len(lines), requests)
	}
	for i, line := range lines {
		var fields map[string]any
		if err := json.Unmarshal([]byte(line), &fields); err != nil {
			t.Fatal(err)
		}
		for _, name := range []string{"method", "path", "target", "status", "duration"} {
			if _, ok := fields[name]; !ok {
				t.Fatalf("missing log field %q", name)
			}
		}
		wantStatus := float64(202)
		if i < 4 {
			wantStatus = 401
		}
		if fields["status"] != wantStatus {
			t.Fatalf("wrong logged status: %v", fields)
		}
		if strings.Contains(line, "real-first") || strings.Contains(line, "real-second") || strings.Contains(line, "virtual") {
			t.Fatal("log leaked credentials")
		}
	}
}

func TestNoFailover(t *testing.T) {
	listener, err := net.Listen("tcp", "127.0.0.1:0")
	if err != nil {
		t.Fatal(err)
	}
	deadURL := "http://" + listener.Addr().String()
	listener.Close()
	var healthyCalls atomic.Int64
	healthy := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		healthyCalls.Add(1)
		w.WriteHeader(204)
	}))
	defer healthy.Close()
	keys := keymap.New(map[string][]config.Target{"virtual": {{BaseURL: deadURL}, {BaseURL: healthy.URL}}})
	srv := New("", keys, slog.New(slog.NewJSONHandler(io.Discard, nil)))
	for i, want := range []int{502, 204} {
		req := httptest.NewRequest("GET", "/anything", nil)
		req.Header.Set("Authorization", "Bearer virtual")
		w := httptest.NewRecorder()
		srv.Handler.ServeHTTP(w, req)
		if w.Code != want || healthyCalls.Load() != int64(i) {
			t.Fatalf("unexpected failover: %d, healthy calls %d", w.Code, healthyCalls.Load())
		}
	}
}

func TestStreamingThroughLogging(t *testing.T) {
	release := make(chan struct{})
	upstream := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		w.Header().Set("Content-Type", "text/event-stream")
		_, _ = io.WriteString(w, "data: first\n\n")
		w.(http.Flusher).Flush()
		select {
		case <-release:
		case <-r.Context().Done():
			return
		}
		_, _ = io.WriteString(w, "data: last\n\n")
	}))
	defer upstream.Close()
	srv := New("", keymap.New(map[string][]config.Target{"virtual": {{BaseURL: upstream.URL}}}), slog.New(slog.NewJSONHandler(io.Discard, nil)))
	gateway := httptest.NewServer(srv.Handler)
	defer gateway.Close()
	defer close(release)
	req, _ := http.NewRequest("GET", gateway.URL+"/stream", nil)
	req.Header.Set("Authorization", "Bearer virtual")
	client := &http.Client{Timeout: 5 * time.Second}
	resp, err := client.Do(req)
	if err != nil {
		t.Fatal(err)
	}
	defer resp.Body.Close()
	line, err := bufio.NewReader(resp.Body).ReadString('\n')
	if err != nil || line != "data: first\n" {
		t.Fatalf("stream buffered by logging wrapper: %q %v", line, err)
	}
}

func TestRunCancelled(t *testing.T) {
	ctx, cancel := context.WithCancel(context.Background())
	cancel()
	srv := New("127.0.0.1:0", keymap.New(nil), slog.New(slog.NewJSONHandler(io.Discard, nil)))
	done := make(chan error, 1)
	go func() { done <- Run(ctx, srv) }()
	select {
	case err := <-done:
		if err != nil {
			t.Fatal(err)
		}
	case <-time.After(5 * time.Second):
		t.Fatal("shutdown did not complete")
	}
}

func TestRunListenFailure(t *testing.T) {
	listener, err := net.Listen("tcp", "127.0.0.1:0")
	if err != nil {
		t.Fatal(err)
	}
	defer listener.Close()
	srv := New(listener.Addr().String(), keymap.New(nil), slog.New(slog.NewJSONHandler(io.Discard, nil)))
	if err := Run(context.Background(), srv); err == nil {
		t.Fatal("listen failure was ignored")
	}
}

func TestResponseWriterInformationalStatus(t *testing.T) {
	w := &responseWriter{ResponseWriter: httptest.NewRecorder()}
	w.WriteHeader(103)
	if w.status != 0 {
		t.Fatal("informational status treated as final")
	}
	w.WriteHeader(201)
	w.WriteHeader(500)
	if w.status != 201 {
		t.Fatal("final status overwritten")
	}
}
