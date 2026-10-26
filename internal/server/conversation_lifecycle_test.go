package server

import (
	"bufio"
	"context"
	"io"
	"net"
	"net/http"
	"net/http/httptest"
	"strings"
	"testing"
	"time"

	"llm-gateway/internal/config"
	"llm-gateway/internal/conversationlog"
)

type panicBody struct{ closed bool }

func (*panicBody) Read([]byte) (int, error) { panic("original panic") }
func (b *panicBody) Close() error           { b.closed = true; return nil }
func TestConversationPanicFinalizesAndRethrows(t *testing.T) {
	srv, dir := conversationServer(t, config.Entry{Target: config.Target{BaseURL: "http://127.0.0.1:1"}}, 8)
	req := conversationRequest("x")
	req.Body = &panicBody{}
	func() {
		defer func() {
			if p := recover(); p != "original panic" {
				t.Fatalf("panic = %v", p)
			}
		}()
		srv.Handler.ServeHTTP(httptest.NewRecorder(), req)
		t.Fatal("panic swallowed")
	}()
	r := readConversations(t, dir, 1)[0]
	if r.Status != 0 || r.ResponseComplete || r.Request.Complete || r.Error != "aborted" {
		t.Fatal(r)
	}
}

type hijackWriter struct {
	header http.Header
	conn   net.Conn
}

func (w *hijackWriter) Header() http.Header         { return w.header }
func (w *hijackWriter) WriteHeader(int)             {}
func (w *hijackWriter) Write(p []byte) (int, error) { return len(p), nil }
func (w *hijackWriter) Hijack() (net.Conn, *bufio.ReadWriter, error) {
	return w.conn, bufio.NewReadWriter(bufio.NewReader(w.conn), bufio.NewWriter(w.conn)), nil
}
func TestConversationHijackIncomplete(t *testing.T) {
	a, b := net.Pipe()
	defer a.Close()
	defer b.Close()
	store := conversationlog.New(t.TempDir(), 8, quietLogger())
	call, err := store.Begin("GET", "/", 0)
	if err != nil {
		t.Fatal(err)
	}
	w := &responseWriter{ResponseWriter: &hijackWriter{header: make(http.Header), conn: a}, call: call}
	if _, _, err := w.Hijack(); err != nil {
		t.Fatal(err)
	}
	r := call.Finish(time.Now(), 0, "", "", "")
	if r.ResponseComplete || r.Error != "unsupported_hijack" {
		t.Fatal(r)
	}
}

func TestConversationGracefulShutdownFinalizes(t *testing.T) {
	started := make(chan struct{})
	release := make(chan struct{})
	upstream := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		_, _ = io.WriteString(w, "first")
		w.(http.Flusher).Flush()
		close(started)
		<-release
		_, _ = io.WriteString(w, "last")
	}))
	defer upstream.Close()
	srv, dir := conversationServer(t, config.Entry{Target: config.Target{BaseURL: upstream.URL}}, 1024)
	ln, err := net.Listen("tcp", "127.0.0.1:0")
	if err != nil {
		t.Fatal(err)
	}
	ctx, cancel := context.WithCancel(context.Background())
	defer cancel()
	done := make(chan error, 1)
	go func() { done <- Serve(ctx, srv, ln) }()
	req, _ := http.NewRequest("POST", "http://"+ln.Addr().String()+"/chat", strings.NewReader("body"))
	req.Header.Set("Authorization", "Bearer virtual")
	client := &http.Client{Timeout: 5 * time.Second}
	defer client.CloseIdleConnections()
	resp, err := client.Do(req)
	if err != nil {
		t.Fatal(err)
	}
	<-started
	cancel()
	select {
	case err := <-done:
		t.Fatalf("shutdown abandoned active stream: %v", err)
	case <-time.After(30 * time.Millisecond):
	}
	close(release)
	body, err := io.ReadAll(resp.Body)
	_ = resp.Body.Close()
	if err != nil || string(body) != "firstlast" {
		t.Fatalf("%q %v", body, err)
	}
	select {
	case err := <-done:
		if err != nil {
			t.Fatal(err)
		}
	case <-time.After(5 * time.Second):
		t.Fatal("shutdown stalled")
	}
	r := readConversations(t, dir, 1)[0]
	if !r.ResponseComplete || r.Response.Data != "firstlast" {
		t.Fatal(r)
	}
}
