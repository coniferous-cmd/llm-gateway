package main

import (
	"context"
	"errors"
	"io"
	"log/slog"
	"net"
	"net/http"
	"net/http/httptest"
	"path/filepath"
	"strings"
	"sync"
	"testing"
	"time"

	"llm-gateway/internal/config"
	"llm-gateway/internal/conversationlog"
	"llm-gateway/internal/e2ee"
	"llm-gateway/internal/keymap"
	"llm-gateway/internal/server"
)

func TestRunAndDrainEncryptedConversation(t *testing.T) {
	logger := slog.New(slog.NewJSONHandler(io.Discard, nil))
	release := make(chan struct{})
	var releaseOnce sync.Once
	releaseStream := func() { releaseOnce.Do(func() { close(release) }) }
	upstream := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		w.Header().Set("Content-Type", "text/event-stream")
		_, _ = io.WriteString(w, "data: first\n\n")
		w.(http.Flusher).Flush()
		select {
		case <-release:
			_, _ = io.WriteString(w, "data: last\n\n")
		case <-r.Context().Done():
		}
	}))
	defer upstream.Close()
	defer releaseStream()
	peerID, err := e2ee.GenerateIdentity()
	if err != nil {
		t.Fatal(err)
	}
	clientID, err := e2ee.GenerateIdentity()
	if err != nil {
		t.Fatal(err)
	}
	plainLn, err := net.Listen("tcp", "127.0.0.1:0")
	if err != nil {
		t.Fatal(err)
	}
	defer plainLn.Close()
	peerLn, err := net.Listen("tcp", "127.0.0.1:0")
	if err != nil {
		t.Fatal(err)
	}
	defer peerLn.Close()
	wrapped := e2ee.NewListener(peerLn, peerID, map[string][]byte{"client": clientID.Public.Public}, logger)
	dir := t.TempDir()
	store := conversationlog.New(dir, 1024, logger)
	keys := keymap.New(map[string][]config.Entry{"virtual": {{Target: config.Target{BaseURL: upstream.URL}}}})
	ordinary := server.NewWithRecorder("", keys, logger, nil, store)
	defer ordinary.Close()
	encrypted := server.NewWithRecorder("", keys, logger, nil, store)
	defer encrypted.Close()
	ctx, cancel := context.WithCancel(context.Background())
	defer cancel()
	peerDone := make(chan struct{})
	peerErr := make(chan error, 1)
	go func() {
		defer close(peerDone)
		err := server.Serve(ctx, encrypted, wrapped)
		peerErr <- err
		if err != nil {
			cancel()
		}
	}()
	ordinaryDone := make(chan struct{})
	allDone := make(chan error, 1)
	go func() {
		allDone <- runAndDrain(func() error { err := server.Serve(ctx, ordinary, plainLn); close(ordinaryDone); return err }, cancel, peerDone)
	}()
	addr := peerLn.Addr().String()
	dialer := e2ee.NewDialer(clientID, map[string][]byte{addr: peerID.Public.Public})
	transport := http.DefaultTransport.(*http.Transport).Clone()
	transport.DialContext = dialer.DialContext
	client := &http.Client{Transport: transport, Timeout: 5 * time.Second}
	defer client.CloseIdleConnections()
	req, _ := http.NewRequest("POST", "http://"+addr+"/v1/chat", strings.NewReader("original"))
	req.Header.Set("Authorization", "Bearer virtual")
	resp, err := client.Do(req)
	if err != nil {
		t.Fatal(err)
	}
	defer resp.Body.Close()
	first := make([]byte, len("data: first\n\n"))
	if _, err := io.ReadFull(resp.Body, first); err != nil {
		t.Fatal(err)
	}
	if string(first) != "data: first\n\n" {
		t.Fatal(string(first))
	}
	cancel()
	select {
	case <-ordinaryDone:
	case <-time.After(3 * time.Second):
		t.Fatal("idle ordinary server did not shut down")
	}
	select {
	case err := <-allDone:
		t.Fatalf("main would exit before encrypted stream finalized: %v", err)
	case <-time.After(50 * time.Millisecond):
	}
	releaseStream()
	rest, err := io.ReadAll(resp.Body)
	if err != nil || string(rest) != "data: last\n\n" {
		t.Fatalf("reply %q: %v", rest, err)
	}
	select {
	case err := <-allDone:
		if err != nil {
			t.Fatal(err)
		}
	case <-time.After(5 * time.Second):
		t.Fatal("coordinator did not finish")
	}
	if err := <-peerErr; err != nil {
		t.Fatal(err)
	}
	// At coordinator return the final file must already exist, not merely become
	// available later. waitConversationRecord also validates its serialized JSON.
	files, err := filepath.Glob(filepath.Join(dir, "*", "*.json"))
	if err != nil || len(files) != 1 {
		t.Fatalf("record missing at process exit: %v %v", files, err)
	}
	record := waitConversationRecord(t, dir)
	if record.Request.Data != "original" || record.Response.Data != "data: first\n\ndata: last\n\n" || !record.ResponseComplete {
		t.Fatalf("record: %+v", record)
	}
}

func TestRunAndDrainStartupFailureCancelsAndWaits(t *testing.T) {
	ctx, cancel := context.WithCancel(context.Background())
	defer cancel()
	peerDone := make(chan struct{})
	release := make(chan struct{})
	defer close(release)
	go func() { <-ctx.Done(); <-release; close(peerDone) }()
	failure := errors.New("listen failed")
	done := make(chan error, 1)
	go func() { done <- runAndDrain(func() error { return failure }, cancel, peerDone) }()
	select {
	case <-ctx.Done():
	case <-time.After(time.Second):
		t.Fatal("peer lifecycle was not canceled")
	}
	select {
	case <-done:
		t.Fatal("returned before peer drain")
	case <-time.After(30 * time.Millisecond):
	}
	release <- struct{}{}
	select {
	case err := <-done:
		if !errors.Is(err, failure) {
			t.Fatalf("lost ordinary error: %v", err)
		}
	case <-time.After(time.Second):
		t.Fatal("did not finish")
	}
}

func TestRunAndDrainWithoutPeer(t *testing.T) {
	canceled := false
	if err := runAndDrain(func() error { return nil }, func() { canceled = true }, nil); err != nil || !canceled {
		t.Fatalf("error=%v canceled=%v", err, canceled)
	}
}
