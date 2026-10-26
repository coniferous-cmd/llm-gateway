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

	"llm-gateway/internal/config"
	"llm-gateway/internal/conversationlog"
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
	return NewWithRecorder(address, keys, logger, peers, nil)
}

// NewWithRecorder optionally captures authenticated calls on either listener.
func NewWithRecorder(address string, keys *keymap.Map, logger *slog.Logger, peers map[string]proxy.PeerDialer, recorder *conversationlog.Store) *http.Server {
	forward := proxy.New(peers)
	handler := http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		started := time.Now()
		response := &responseWriter{ResponseWriter: w}
		route, ok := keys.Select(r.Header.Get("Authorization"))
		targetURL, model := "", config.Model{}
		var call *conversationlog.Call
		defer func() {
			aborted := recover()
			status := response.status
			if status == 0 && aborted == nil && !response.hijacked {
				status = http.StatusOK
			}
			if call != nil {
				if aborted != nil {
					call.Fail("aborted")
				}
				if r.Context().Err() != nil {
					call.Fail("canceled")
				}
				recorder.Finish(call, status, targetURL, model.ID, model.Name)
			}
			fields := []any{"method", r.Method, "path", r.URL.Path,
				"target", targetURL, "status", status, "duration", time.Since(started)}
			if model.ID != "" {
				fields = append(fields, slog.Group("model", "id", model.ID, "name", model.Name))
			}
			if call != nil {
				fields = append(fields, "request_id", call.ID())
			}
			logger.Info("request", fields...)
			if aborted != nil {
				panic(aborted)
			}
		}()
		if !ok {
			http.Error(response, "Unauthorized", http.StatusUnauthorized)
			return
		}
		if recorder != nil {
			call, _ = recorder.Begin(r.Method, r.URL.EscapedPath(), r.ContentLength)
			if call != nil {
				response.call = call
				if r.Body != nil {
					r.Body = call.Request.Wrap(r.Body)
				}
				r = r.WithContext(conversationlog.WithCall(r.Context(), call))
			}
		}
		target, matched, err := proxy.Prepare(r, route)
		if err != nil {
			call.Fail("preparation_failed")
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
	status   int
	call     *conversationlog.Call
	hijacked bool
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
	n, err := w.ResponseWriter.Write(data)
	if w.call != nil {
		w.call.Response.Observe(data[:n], err)
		if err != nil {
			w.call.Fail("client_write")
		}
	}
	return n, err
}

func (w *responseWriter) Flush() {
	if w.status == 0 {
		w.WriteHeader(http.StatusOK)
	}
	err := http.NewResponseController(w.ResponseWriter).Flush()
	if err != nil {
		w.call.Fail("client_write")
	}
}

func (w *responseWriter) Hijack() (net.Conn, *bufio.ReadWriter, error) {
	conn, rw, err := http.NewResponseController(w.ResponseWriter).Hijack()
	if err == nil {
		w.hijacked = true
		w.call.Fail("unsupported_hijack")
	}
	return conn, rw, err
}
