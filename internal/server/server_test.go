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
	keys := keymap.New(map[string][]config.Entry{"virtual": {
		{Target: config.Target{BaseURL: first.URL, APIKey: "real-first"}},
		{Target: config.Target{BaseURL: second.URL, APIKey: "real-second"}},
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

func TestPassthroughModelLogging(t *testing.T) {
	const body = `{"model":"provider-model","messages":[]}`
	upstream := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		data, _ := io.ReadAll(r.Body)
		if string(data) != body {
			t.Errorf("body changed: %q", data)
		}
		w.WriteHeader(http.StatusNoContent)
	}))
	defer upstream.Close()
	var logs bytes.Buffer
	keys := keymap.New(map[string][]config.Entry{"virtual": {{Target: config.Target{BaseURL: upstream.URL}}}})
	srv := New(config.DefaultListen, keys, slog.New(slog.NewJSONHandler(&logs, nil)))
	req := httptest.NewRequest("POST", "/v1/chat/completions", strings.NewReader(body))
	req.Header.Set("Authorization", "Bearer virtual")
	w := httptest.NewRecorder()
	srv.Handler.ServeHTTP(w, req)
	if w.Code != http.StatusNoContent {
		t.Fatalf("status = %d", w.Code)
	}
	var fields struct {
		Model config.Model `json:"model"`
	}
	if err := json.Unmarshal(logs.Bytes(), &fields); err != nil {
		t.Fatal(err)
	}
	if fields.Model.ID != "provider-model" || fields.Model.Name != "provider-model" {
		t.Fatalf("missing passthrough model: %s", logs.String())
	}
}

func TestModelBasedRouting(t *testing.T) {
	type record struct {
		calls int
		body  string
	}
	upstream := func(rec *record) *httptest.Server {
		return httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
			rec.calls++
			data, _ := io.ReadAll(r.Body)
			rec.body = string(data)
			w.WriteHeader(http.StatusOK)
		}))
	}
	flashRec, proRec := &record{}, &record{}
	flash, pro := upstream(flashRec), upstream(proRec)
	defer flash.Close()
	defer pro.Close()
	var logs bytes.Buffer
	keys := keymap.New(map[string][]config.Entry{"virtual": {
		{Target: config.Target{BaseURL: flash.URL, APIKey: "real-flash"}, Models: []config.Model{{ID: "mimo-v2.6-flash", Name: "mimo-v2.6-flash"}}},
		{Target: config.Target{BaseURL: pro.URL, APIKey: "real-pro"}, Models: []config.Model{{ID: "alias-pro", Name: "mimo-v2.6-pro"}}},
	}})
	srv := New(config.DefaultListen, keys, slog.New(slog.NewJSONHandler(&logs, nil)))
	send := func(method, body string) *httptest.ResponseRecorder {
		t.Helper()
		req := httptest.NewRequest(method, "/v1/chat/completions", strings.NewReader(body))
		req.Header.Set("Authorization", "Bearer virtual")
		w := httptest.NewRecorder()
		srv.Handler.ServeHTTP(w, req)
		return w
	}
	// The model id picks the entry; the alias is rewritten to the provider name.
	if w := send("POST", `{"model":"mimo-v2.6-flash","n":1}`); w.Code != 200 || flashRec.calls != 1 || proRec.calls != 0 {
		t.Fatalf("flash routing: %d, calls %d/%d", w.Code, flashRec.calls, proRec.calls)
	}
	if flashRec.body != `{"model":"mimo-v2.6-flash","n":1}` {
		t.Fatalf("flash body = %q", flashRec.body)
	}
	if w := send("POST", `{"model":"alias-pro"}`); w.Code != 200 || proRec.calls != 1 || flashRec.calls != 1 {
		t.Fatalf("pro routing: %d, calls %d/%d", w.Code, flashRec.calls, proRec.calls)
	}
	if proRec.body != `{"model":"mimo-v2.6-pro"}` {
		t.Fatalf("pro body = %q (alias not rewritten)", proRec.body)
	}
	// An unknown model id is rejected without contacting an upstream.
	if w := send("POST", `{"model":"typo"}`); w.Code != 400 || flashRec.calls+proRec.calls != 2 {
		t.Fatalf("unknown model: %d, calls %d/%d", w.Code, flashRec.calls, proRec.calls)
	}
	// Requests without a resolvable model fall back to round-robin untouched.
	for _, want := range []*record{flashRec, proRec} {
		before := want.calls
		if w := send("GET", ""); w.Code != 200 || want.calls != before+1 {
			t.Fatalf("model-less request: %d, calls %d/%d", w.Code, flashRec.calls, proRec.calls)
		}
	}
	// The log carries the model id and name only for matched requests:
	// line 0 is flash (id == name), line 1 is the alias-pro rewrite.
	wantModels := [][2]string{{"mimo-v2.6-flash", "mimo-v2.6-flash"}, {"alias-pro", "mimo-v2.6-pro"}}
	for i, line := range strings.Split(strings.TrimSpace(logs.String()), "\n") {
		var fields map[string]any
		if err := json.Unmarshal([]byte(line), &fields); err != nil {
			t.Fatal(err)
		}
		_, logged := fields["model"]
		if want := i < 2; logged != want {
			t.Fatalf("line %d model logging = %v: %s", i, logged, line)
		}
		if logged {
			model, _ := fields["model"].(map[string]any)
			id, _ := model["id"].(string)
			name, _ := model["name"].(string)
			if id != wantModels[i][0] || name != wantModels[i][1] {
				t.Fatalf("line %d model = {id:%q name:%q}, want %q/%q", i, id, name, wantModels[i][0], wantModels[i][1])
			}
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
	keys := keymap.New(map[string][]config.Entry{"virtual": {{Target: config.Target{BaseURL: deadURL}}, {Target: config.Target{BaseURL: healthy.URL}}}})
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
	srv := New("", keymap.New(map[string][]config.Entry{"virtual": {{Target: config.Target{BaseURL: upstream.URL}}}}), slog.New(slog.NewJSONHandler(io.Discard, nil)))
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

// Shutdown must drain an in-flight SSE stream instead of severing it: cancel
// while the first event is on the wire, then the client must still receive
// everything the upstream sends afterwards and Run must report success.
func TestRunDrainsActiveStream(t *testing.T) {
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
	defer close(release)
	listener, err := net.Listen("tcp", "127.0.0.1:0")
	if err != nil {
		t.Fatal(err)
	}
	address := listener.Addr().String()
	listener.Close()
	keys := keymap.New(map[string][]config.Entry{"virtual": {{Target: config.Target{BaseURL: upstream.URL}}}})
	ctx, cancel := context.WithCancel(context.Background())
	defer cancel()
	srv := New(address, keys, slog.New(slog.NewJSONHandler(io.Discard, nil)))
	done := make(chan error, 1)
	go func() { done <- Run(ctx, srv) }()
	client := &http.Client{Timeout: 10 * time.Second}
	defer client.CloseIdleConnections()
	deadline := time.Now().Add(5 * time.Second)
	var resp *http.Response
	for {
		req, _ := http.NewRequest("GET", "http://"+address+"/stream", nil)
		req.Header.Set("Authorization", "Bearer virtual")
		resp, err = client.Do(req)
		if err == nil {
			break
		}
		if time.Now().After(deadline) {
			t.Fatalf("gateway never started: %v", err)
		}
		time.Sleep(25 * time.Millisecond)
	}
	defer resp.Body.Close()
	line, err := bufio.NewReader(resp.Body).ReadString('\n')
	if err != nil || line != "data: first\n" {
		t.Fatalf("first event: %q %v", line, err)
	}
	// Shutdown begins while the stream is active; the listener must close.
	cancel()
	deadline = time.Now().Add(5 * time.Second)
	for {
		conn, err := net.DialTimeout("tcp", address, 250*time.Millisecond)
		if err != nil {
			break
		}
		conn.Close()
		if time.Now().After(deadline) {
			t.Fatal("listener still open after cancel")
		}
		time.Sleep(25 * time.Millisecond)
	}
	release <- struct{}{}
	rest, err := io.ReadAll(resp.Body)
	if err != nil || string(rest) != "data: last\n\n" {
		t.Fatalf("drained tail: %q %v", rest, err)
	}
	select {
	case err := <-done:
		if err != nil {
			t.Fatalf("Run after drain: %v", err)
		}
	case <-time.After(15 * time.Second):
		t.Fatal("shutdown did not finish after the stream drained")
	}
}

// A first Write or Flush without WriteHeader must record the implicit 200.
func TestResponseWriterImplicitOK(t *testing.T) {
	rec := httptest.NewRecorder()
	w := &responseWriter{ResponseWriter: rec}
	if _, err := w.Write([]byte("body")); err != nil {
		t.Fatal(err)
	}
	if w.status != http.StatusOK || rec.Code != http.StatusOK || rec.Body.String() != "body" {
		t.Fatalf("status %d, recorder %d, body %q", w.status, rec.Code, rec.Body.String())
	}
	rec = httptest.NewRecorder()
	w = &responseWriter{ResponseWriter: rec}
	w.Flush()
	if w.status != http.StatusOK || !rec.Flushed {
		t.Fatalf("status %d, flushed %v", w.status, rec.Flushed)
	}
}

func TestResponseWriterUnwrap(t *testing.T) {
	rec := httptest.NewRecorder()
	w := &responseWriter{ResponseWriter: rec}
	if w.Unwrap() != rec {
		t.Fatal("Unwrap does not expose the underlying writer")
	}
}

func TestResponseWriterHijack(t *testing.T) {
	fake := &hijackRecorder{ResponseWriter: httptest.NewRecorder()}
	w := &responseWriter{ResponseWriter: fake}
	conn, _, err := w.Hijack()
	if err != nil || conn != nil || !fake.hijacked {
		t.Fatalf("Hijack = %v %v, hijacked %v", conn, err, fake.hijacked)
	}
}

// hijackRecorder is the minimal http.Hijacker the ResponseController needs.
type hijackRecorder struct {
	http.ResponseWriter
	hijacked bool
}

func (h *hijackRecorder) Hijack() (net.Conn, *bufio.ReadWriter, error) {
	h.hijacked = true
	return nil, nil, nil
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
