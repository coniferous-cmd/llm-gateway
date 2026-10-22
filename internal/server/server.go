// Package server assembles authentication, request logging and HTTP lifecycle.
package server

import (
	"bufio"
	"context"
	"errors"
	"log/slog"
	"net"
	"net/http"
	"time"

	"llm-gateway/internal/keymap"
	"llm-gateway/internal/proxy"
)

func New(address string, keys *keymap.Map, logger *slog.Logger) *http.Server {
	return NewWithPeers(address, keys, logger, nil)
}

// NewWithPeers builds the HTTP server for the client-facing (or e2ee) listen
// address with the same handler pipeline. peers wires the proxy transport to
// Noise dialers for e2ee destinations; nil means plaintext only. The e2ee
// listener reuses this identical handler so decrypted peer requests flow
// through the exact same auth, logging and forwarding path as local callers.
func NewWithPeers(address string, keys *keymap.Map, logger *slog.Logger, peers map[string]proxy.PeerDialer) *http.Server {
	forward := proxy.New(peers)
	handler := http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		started := time.Now()
		response := &responseWriter{ResponseWriter: w}
		route, ok := keys.Select(r.Header.Get("Authorization"))
		targetURL, model := "", ""
		defer func() {
			status := response.status
			if status == 0 {
				status = http.StatusOK
			}
			fields := []any{"method", r.Method, "path", r.URL.Path,
				"target", targetURL, "status", status, "duration", time.Since(started)}
			if model != "" {
				fields = append(fields, "model", model)
			}
			logger.Info("request", fields...)
		}()
		if !ok {
			http.Error(response, "Unauthorized", http.StatusUnauthorized)
			return
		}
		target, matched, err := proxy.Prepare(r, route)
		if err != nil {
			status := http.StatusBadRequest
			if errors.Is(err, proxy.ErrBodyTooLarge) {
				status = http.StatusRequestEntityTooLarge
			}
			http.Error(response, http.StatusText(status), status)
			return
		}
		targetURL, model = target.BaseURL, matched
		forward.ServeHTTP(response, r.WithContext(keymap.WithTarget(r.Context(), target)))
	})
	return &http.Server{Addr: address, Handler: handler, ReadHeaderTimeout: 10 * time.Second, IdleTimeout: 90 * time.Second}
}

// Run waits for shutdown and drains active requests, including streams, for 10s.
func Run(ctx context.Context, srv *http.Server) error {
	return run(ctx, srv, srv.ListenAndServe)
}

// Serve runs the server on an already-listening socket (e.g. an e2ee-wrapped
// listener) with the same drain semantics as Run.
func Serve(ctx context.Context, srv *http.Server, ln net.Listener) error {
	return run(ctx, srv, func() error { return srv.Serve(ln) })
}

func run(ctx context.Context, srv *http.Server, serve func() error) error {
	done := make(chan error, 1)
	go func() { done <- serve() }()
	select {
	case err := <-done:
		if errors.Is(err, http.ErrServerClosed) {
			return nil
		}
		return err
	case <-ctx.Done():
		shutdownCtx, cancel := context.WithTimeout(context.Background(), 10*time.Second)
		defer cancel()
		if err := srv.Shutdown(shutdownCtx); err != nil {
			_ = srv.Close()
			return err
		}
		err := <-done
		if errors.Is(err, http.ErrServerClosed) {
			return nil
		}
		return err
	}
}

type responseWriter struct {
	http.ResponseWriter
	status int
}

func (w *responseWriter) Unwrap() http.ResponseWriter { return w.ResponseWriter }

func (w *responseWriter) WriteHeader(status int) {
	if w.status != 0 {
		return
	}
	if status >= 200 {
		w.status = status
	}
	w.ResponseWriter.WriteHeader(status)
}

func (w *responseWriter) Write(data []byte) (int, error) {
	if w.status == 0 {
		w.WriteHeader(http.StatusOK)
	}
	return w.ResponseWriter.Write(data)
}

func (w *responseWriter) Flush() {
	if w.status == 0 {
		w.WriteHeader(http.StatusOK)
	}
	_ = http.NewResponseController(w.ResponseWriter).Flush()
}

func (w *responseWriter) Hijack() (net.Conn, *bufio.ReadWriter, error) {
	return http.NewResponseController(w.ResponseWriter).Hijack()
}
