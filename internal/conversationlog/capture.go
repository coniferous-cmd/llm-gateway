// Package conversationlog captures bounded opaque HTTP body prefixes and stores
// one versioned record per authenticated call. It never interprets API payloads.
package conversationlog

import (
	"context"
	"encoding/base64"
	"io"
	"sync"
	"time"
	"unicode/utf8"
)

type Body struct {
	Data          string `json:"data"`
	Encoding      string `json:"encoding"`
	ObservedBytes int64  `json:"observed_bytes"`
	CapturedBytes int    `json:"captured_bytes"`
	Truncated     bool   `json:"truncated"`
	Complete      bool   `json:"complete"`
}

type Record struct {
	SchemaVersion    int       `json:"schema_version"`
	RequestID        string    `json:"request_id"`
	StartedAt        time.Time `json:"started_at"`
	EndedAt          time.Time `json:"ended_at"`
	DurationMS       int64     `json:"duration_ms"`
	Method           string    `json:"method"`
	Path             string    `json:"path"`
	Upstream         string    `json:"upstream,omitempty"`
	ModelID          string    `json:"model_id,omitempty"`
	ModelName        string    `json:"model_name,omitempty"`
	Status           int       `json:"status,omitempty"`
	ResponseComplete bool      `json:"response_complete"`
	Error            string    `json:"error,omitempty"`
	Request          Body      `json:"request"`
	Response         Body      `json:"response"`
}

// Capture can be observed and snapshotted while a transport reader is active.
// It allocates only as bytes arrive, and never retains more than limit bytes.
type Capture struct {
	mu          sync.Mutex
	limit       int
	data        []byte
	observed    int64
	expected    int64
	eof, failed bool
}

func NewCapture(limit int, expected int64) *Capture {
	return &Capture{limit: limit, expected: expected}
}

func (c *Capture) Observe(p []byte, err error) {
	c.mu.Lock()
	defer c.mu.Unlock()
	c.observed += int64(len(p))
	n := min(len(p), c.limit-len(c.data))
	c.data = append(c.data, p[:n]...)
	if err == io.EOF {
		c.eof = true
	} else if err != nil {
		c.failed = true
	}
}

func (c *Capture) Snapshot() Body {
	c.mu.Lock()
	defer c.mu.Unlock()
	b := Body{Encoding: "utf-8", ObservedBytes: c.observed, CapturedBytes: len(c.data), Truncated: c.observed > int64(len(c.data)), Complete: !c.failed && (c.eof || (c.expected >= 0 && c.observed == c.expected))}
	if utf8.Valid(c.data) {
		b.Data = string(c.data)
	} else {
		b.Encoding = "base64"
		b.Data = base64.StdEncoding.EncodeToString(c.data)
	}
	return b
}

type capturedReader struct {
	io.ReadCloser
	capture *Capture
}

func (r *capturedReader) Read(p []byte) (int, error) {
	n, err := r.ReadCloser.Read(p)
	r.capture.Observe(p[:n], err)
	return n, err
}
func (c *Capture) Wrap(body io.ReadCloser) io.ReadCloser {
	return &capturedReader{ReadCloser: body, capture: c}
}

// Call holds per-request state shared with ReverseProxy through context.
type Call struct {
	Request, Response *Capture
	record            Record
	mu                sync.Mutex
	category          string
}

type callKey struct{}

func WithCall(ctx context.Context, c *Call) context.Context {
	return context.WithValue(ctx, callKey{}, c)
}
func FromContext(ctx context.Context) *Call { c, _ := ctx.Value(callKey{}).(*Call); return c }

// Fail accepts only fixed categories so transport errors cannot leak secrets.
func (c *Call) Fail(category string) {
	if c == nil {
		return
	}
	switch category {
	case "preparation_failed", "upstream_transport", "upstream_read", "client_write", "canceled", "aborted", "unsupported_hijack":
	default:
		return
	}
	c.mu.Lock()
	defer c.mu.Unlock()
	if c.category == "" || c.category == "preparation_failed" || c.category == "upstream_transport" {
		c.category = category
	}
}

// ObserveUpstream reads only when ReverseProxy reads; it does not buffer data.
func (c *Call) ObserveUpstream(body io.ReadCloser) io.ReadCloser {
	return &upstreamReader{ReadCloser: body, call: c}
}

type upstreamReader struct {
	io.ReadCloser
	call *Call
}

func (r *upstreamReader) Read(p []byte) (int, error) {
	n, err := r.ReadCloser.Read(p)
	if err != nil && err != io.EOF {
		r.call.Fail("upstream_read")
	}
	return n, err
}

func (c *Call) ID() string { return c.record.RequestID }
func (c *Call) Finish(ended time.Time, status int, upstream, modelID, modelName string) Record {
	c.mu.Lock()
	category := c.category
	c.mu.Unlock()
	r := c.record
	r.EndedAt = ended.UTC()
	r.DurationMS = ended.Sub(r.StartedAt).Milliseconds()
	r.Status = status
	r.Upstream = upstream
	r.ModelID = modelID
	r.ModelName = modelName
	r.Error = category
	r.Request = c.Request.Snapshot()
	r.Response = c.Response.Snapshot()
	// A normal handler return establishes completion for locally-generated and
	// proxied responses. Upstream read failures and panics are explicitly observed.
	r.Response.Complete = category == "" || category == "preparation_failed" || category == "upstream_transport"
	r.ResponseComplete = r.Response.Complete
	return r
}
