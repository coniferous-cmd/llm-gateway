// Package keymap authenticates virtual keys and selects immutable targets.
package keymap

import (
	"context"
	"strings"
	"sync/atomic"

	"llm-gateway/internal/config"
)

type route struct {
	targets []config.Target
	next    atomic.Uint64
}

type Map struct{ routes map[string]*route }

func New(keys map[string][]config.Target) *Map {
	m := &Map{routes: make(map[string]*route, len(keys))}
	for token, targets := range keys {
		if len(targets) != 0 {
			m.routes[token] = &route{targets: append([]config.Target(nil), targets...)}
		}
	}
	return m
}

// Select advances the counter only after successful Bearer authentication.
func (m *Map) Select(authorization string) (config.Target, bool) {
	fields := strings.Fields(authorization)
	if len(fields) != 2 || !strings.EqualFold(fields[0], "Bearer") {
		return config.Target{}, false
	}
	r, ok := m.routes[fields[1]]
	if !ok {
		return config.Target{}, false
	}
	index := (r.next.Add(1) - 1) % uint64(len(r.targets))
	return r.targets[index], true
}

type targetKey struct{}

func WithTarget(ctx context.Context, target config.Target) context.Context {
	return context.WithValue(ctx, targetKey{}, target)
}

func TargetFromContext(ctx context.Context) (config.Target, bool) {
	target, ok := ctx.Value(targetKey{}).(config.Target)
	return target, ok
}
