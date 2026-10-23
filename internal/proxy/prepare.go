package proxy

import (
	"bytes"
	"errors"
	"fmt"
	"io"
	"net/http"
	"strings"

	"llm-gateway/internal/config"
	"llm-gateway/internal/keymap"
)

// maxInspectedBody bounds how much of a request body model routing reads.
const maxInspectedBody = 32 << 20

var (
	// ErrUnknownModel rejects a model id no entry of the token serves.
	ErrUnknownModel = errors.New("unknown model id")
	// ErrBodyTooLarge rejects bodies beyond the inspection limit.
	ErrBodyTooLarge = errors.New("request body too large")
)

// Prepare selects the forwarding target for an authenticated request: the
// entry declaring the body's top-level "model" string when the token declares
// models, round-robin otherwise. On a match with id != name the body's model
// literal is rewritten to name in place. The returned Model carries both the
// caller's model id and upstream name (equal for passthrough tokens), or its
// zero value when no model could be inspected.
func Prepare(r *http.Request, route *keymap.Route) (config.Target, config.Model, error) {
	hasModels := route.HasModels()
	body, err := inspectBody(r, hasModels)
	if err != nil {
		return config.Target{}, config.Model{}, err
	}
	if body == nil {
		return route.RoundRobin(), config.Model{}, nil
	}
	id, start, end, ok := modelValue(body)
	if !ok {
		return route.RoundRobin(), config.Model{}, nil
	}
	if !hasModels {
		return route.RoundRobin(), config.Model{ID: id, Name: id}, nil
	}
	target, model, ok := route.Resolve(id)
	if !ok {
		return config.Target{}, config.Model{}, fmt.Errorf("%w: %q", ErrUnknownModel, id)
	}
	if model.ID == model.Name {
		return target, model, nil
	}
	rewritten := spliceModelValue(body, start, end, model.Name)
	r.Body = io.NopCloser(bytes.NewReader(rewritten))
	r.ContentLength = int64(len(rewritten))
	r.Header.Del("Content-Length")
	return target, model, nil
}

// inspectBody buffers the request body so the model field can be read and
// possibly rewritten, and installs the buffered bytes as the new request body.
// It returns nil when the body must not be inspected (empty or non-identity
// content encoding) or cannot be (a read error mid-body: the partial body is
// replayed untouched). Beyond the limit, model routing fails with
// ErrBodyTooLarge; logging-only inspection replays the body without rejecting it.
func inspectBody(r *http.Request, enforceLimit bool) ([]byte, error) {
	if r.Body == nil || r.ContentLength == 0 || encoded(r) || (!enforceLimit && r.ContentLength > maxInspectedBody) {
		return nil, nil
	}
	data, err := io.ReadAll(io.LimitReader(r.Body, maxInspectedBody+1))
	if err != nil {
		r.Body = replay{Reader: io.MultiReader(bytes.NewReader(data), r.Body), Closer: r.Body}
		return nil, nil
	}
	if len(data) > maxInspectedBody && !enforceLimit {
		r.Body = replay{Reader: io.MultiReader(bytes.NewReader(data), r.Body), Closer: r.Body}
		return nil, nil
	}
	_ = r.Body.Close()
	if len(data) > maxInspectedBody {
		return nil, ErrBodyTooLarge
	}
	r.Body = io.NopCloser(bytes.NewReader(data))
	r.ContentLength = int64(len(data))
	r.Header.Del("Content-Length")
	return data, nil
}

// encoded reports whether the body carries a non-identity content encoding and
// therefore must pass through uninspected and unmodified.
func encoded(r *http.Request) bool {
	encoding := strings.TrimSpace(r.Header.Get("Content-Encoding"))
	return encoding != "" && !strings.EqualFold(encoding, "identity")
}

// replay restores a partially read body together with its original closer.
type replay struct {
	io.Reader
	io.Closer
}
