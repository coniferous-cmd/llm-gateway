// Package keymap authenticates virtual keys and selects immutable targets.
package keymap

import (
	"context"
	"strings"
	"sync/atomic"
	"time"

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
		for i := range r.entries {
			// Own the rule slice so later changes to the startup input cannot
			// change either model selection or round-robin forwarding.
			r.entries[i].Target.Rewrite = append([]config.RewriteRule(nil), r.entries[i].Target.Rewrite...)
			entry := r.entries[i]
			for _, model := range entry.Models {
				r.byModel[model.ID] = modelTarget{target: entry.Target, model: model}
			}
		}
		m.routes[token] = r
	}
	return m
}

// expired reports whether the target's expiry timestamp, when set, has passed.
func expired(t config.Target) bool {
	return !t.Expire.IsZero() && !time.Now().Before(t.Expire.Time)
}

// HasLive reports whether any entry of the token is not past its expire time.
func (r *Route) HasLive() bool {
	for i := range r.entries {
		if !expired(r.entries[i].Target) {
			return true
		}
	}
	return false
}

// Select authenticates the Bearer header and returns the token's route. A
// token whose every entry has expired authenticates as an unknown token: the
// round-robin counter advances only in RoundRobin, so model-matched and
// rejected requests never shift the rotation.
func (m *Map) Select(authorization string) (*Route, bool) {
	fields := strings.Fields(authorization)
	if len(fields) != 2 || !strings.EqualFold(fields[0], "Bearer") {
		return nil, false
	}
	r, ok := m.routes[fields[1]]
	if !ok || !r.HasLive() {
		return nil, false
	}
	return r, true
}

// HasModels reports whether the token declares model ids for model routing.
func (r *Route) HasModels() bool { return len(r.byModel) != 0 }

// longContextSuffix is Claude Code's 1M-context marker on a model id. The base
// id serves the suffixed variant, with the marker kept on the upstream name.
const longContextSuffix = "[1m]"

// Resolve maps a caller's model id onto the entry declaring it. A verbatim id
// wins; otherwise the id without longContextSuffix is tried and the suffix is
// appended to the entry's upstream name.
func (r *Route) Resolve(id string) (config.Target, config.Model, bool) {
	mt, ok := r.byModel[id]
	if !ok {
		base, found := strings.CutSuffix(id, longContextSuffix)
		mt, ok = r.byModel[base]
		if !found || !ok {
			return config.Target{}, config.Model{}, false
		}
		mt.model = config.Model{ID: id, Name: mt.model.Name + longContextSuffix}
	}
	if expired(mt.target) {
		return config.Target{}, config.Model{}, false
	}
	return mt.target, mt.model, true
}

// RoundRobin advances the counter for requests without a model match, cycling
// through entries that are not past their expire time.
func (r *Route) RoundRobin() config.Target {
	n := 0
	for i := range r.entries {
		if !expired(r.entries[i].Target) {
			r.entries[n] = r.entries[i]
			n++
		}
	}
	index := (r.next.Add(1) - 1) % uint64(n)
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
