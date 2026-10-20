// Package keymap authenticates virtual keys and selects immutable targets.
package keymap

import (
	"context"
	"strings"
	"sync/atomic"

	"llm-gateway/internal/config"
)

// Route is the immutable entry list of one virtual key plus its routing state.
type Route struct {
	entries []config.Entry
	byModel map[string]modelTarget
	next    atomic.Uint64
}

type modelTarget struct {
	target config.Target
	model  config.Model
}

type Map struct{ routes map[string]*Route }

func New(keys map[string][]config.Entry) *Map {
	m := &Map{routes: make(map[string]*Route, len(keys))}
	for token, entries := range keys {
		if len(entries) == 0 {
			continue
		}
		r := &Route{entries: append([]config.Entry(nil), entries...), byModel: make(map[string]modelTarget)}
		for _, entry := range r.entries {
			for _, model := range entry.Models {
				r.byModel[model.ID] = modelTarget{target: entry.Target, model: model}
			}
		}
		m.routes[token] = r
	}
	return m
}

// Select authenticates the Bearer header and returns the token's route. The
// round-robin counter advances only in RoundRobin, so model-matched and
// rejected requests never shift the rotation.
func (m *Map) Select(authorization string) (*Route, bool) {
	fields := strings.Fields(authorization)
	if len(fields) != 2 || !strings.EqualFold(fields[0], "Bearer") {
		return nil, false
	}
	r, ok := m.routes[fields[1]]
	return r, ok
}

// HasModels reports whether the token declares model ids for model routing.
func (r *Route) HasModels() bool { return len(r.byModel) != 0 }

// Resolve maps a caller's model id onto the entry declaring it. Config
// validation keeps ids unique per token, so the match is unambiguous.
func (r *Route) Resolve(id string) (config.Target, config.Model, bool) {
	mt, ok := r.byModel[id]
	return mt.target, mt.model, ok
}

// RoundRobin advances the counter for requests without a model match.
func (r *Route) RoundRobin() config.Target {
	index := (r.next.Add(1) - 1) % uint64(len(r.entries))
	return r.entries[index].Target
}

type targetKey struct{}

func WithTarget(ctx context.Context, target config.Target) context.Context {
	return context.WithValue(ctx, targetKey{}, target)
}

func TargetFromContext(ctx context.Context) (config.Target, bool) {
	target, ok := ctx.Value(targetKey{}).(config.Target)
	return target, ok
}
