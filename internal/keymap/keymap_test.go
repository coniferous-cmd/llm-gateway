package keymap

import (
	"context"
	"fmt"
	"reflect"
	"sync"
	"sync/atomic"
	"testing"

	"llm-gateway/internal/config"
)

func TestAuthentication(t *testing.T) {
	m := New(map[string][]config.Entry{"dev": {{Target: config.Target{BaseURL: "one"}}}, "empty": nil})
	for _, header := range []string{"", "Basic dev", "Bearer", "Bearer unknown", "Bearer dev extra", "Bearer empty"} {
		if _, ok := m.Select(header); ok {
			t.Fatalf("accepted %q", header)
		}
	}
	for i := 0; i < 6; i++ {
		route, ok := m.Select("bearer dev")
		if !ok || route.RoundRobin().BaseURL != "one" {
			t.Fatal(route, ok)
		}
	}
}

func TestRoundRobin(t *testing.T) {
	entries := []config.Entry{
		{Target: config.Target{BaseURL: "1"}},
		{Target: config.Target{BaseURL: "2"}},
		{Target: config.Target{BaseURL: "3"}},
	}
	m := New(map[string][]config.Entry{"dev": entries, "other": entries})
	entries[0].Target.BaseURL = "mutated"
	for i := 0; i < 6; i++ {
		route, ok := m.Select("Bearer dev")
		if !ok || route.RoundRobin().BaseURL != fmt.Sprint(i%3+1) {
			t.Fatal("unbalanced rotation")
		}
	}
	route, _ := m.Select("Bearer other")
	if route.RoundRobin().BaseURL != "1" {
		t.Fatal("counters are not independent")
	}
}

func TestModelResolution(t *testing.T) {
	m := New(map[string][]config.Entry{"dev": {
		{Target: config.Target{BaseURL: "flash"}, Models: []config.Model{{ID: "f", Name: "flash-real"}}},
		{Target: config.Target{BaseURL: "pro"}, Models: []config.Model{{ID: "p", Name: "pro-real"}}},
	}})
	route, ok := m.Select("Bearer dev")
	if !ok || !route.HasModels() {
		t.Fatal("token with models not flagged")
	}
	target, model, ok := route.Resolve("f")
	if !ok || target.BaseURL != "flash" || model != (config.Model{ID: "f", Name: "flash-real"}) {
		t.Fatal(target, model, ok)
	}
	if _, _, ok := route.Resolve("unknown"); ok {
		t.Fatal("unknown model id resolved")
	}
	// Matched picks must leave the round-robin rotation untouched.
	for i := 0; i < 3; i++ {
		route.Resolve("p")
	}
	for _, want := range []string{"flash", "pro"} {
		if got := route.RoundRobin().BaseURL; got != want {
			t.Fatalf("rotation shifted by model matches: got %q, want %q", got, want)
		}
	}
	legacy, _ := m.Select("Bearer none")
	if legacy != nil {
		t.Fatal("unexpected route")
	}
}

func TestLongContextSuffixResolution(t *testing.T) {
	m := New(map[string][]config.Entry{"dev": {
		{Target: config.Target{BaseURL: "haiku"}, Models: []config.Model{{ID: "claude-haiku-5-5", Name: "deepseek-v4.1-flash"}}},
		{Target: config.Target{BaseURL: "explicit"}, Models: []config.Model{
			{ID: "x[1m]", Name: "y[1m]"},
		}},
	}})
	route, _ := m.Select("Bearer dev")

	target, model, ok := route.Resolve("claude-haiku-5-5[1m]")
	if want := (config.Model{ID: "claude-haiku-5-5[1m]", Name: "deepseek-v4.1-flash[1m]"}); !ok || target.BaseURL != "haiku" || model != want {
		t.Fatalf("suffixed resolve = %q %+v %v, want %q %+v", target.BaseURL, model, ok, "haiku", want)
	}
	// A verbatim declaration wins over the base-id fallback.
	if target, model, ok := route.Resolve("x[1m]"); !ok || target.BaseURL != "explicit" || model != (config.Model{ID: "x[1m]", Name: "y[1m]"}) {
		t.Fatalf("explicit suffixed resolve = %q %+v %v", target.BaseURL, model, ok)
	}
	for _, id := range []string{"unknown[1m]", "[1m]", "claude-haiku-5-5[1M]"} {
		if _, _, ok := route.Resolve(id); ok {
			t.Fatalf("resolved %q", id)
		}
	}
}

func TestLegacyTokenHasNoModels(t *testing.T) {
	m := New(map[string][]config.Entry{"dev": {{Target: config.Target{BaseURL: "one"}, Models: nil}}})
	route, _ := m.Select("Bearer dev")
	if route.HasModels() {
		t.Fatal("legacy token inspected for models")
	}
}

func TestTargetContext(t *testing.T) {
	if _, ok := TargetFromContext(context.Background()); ok {
		t.Fatal("empty context reports a target")
	}
	want := config.Target{BaseURL: "https://api.example", APIKey: "real", Rewrite: []config.RewriteRule{{Source: "/api", Destination: "/v1"}}}
	got, ok := TargetFromContext(WithTarget(context.Background(), want))
	if !ok || !reflect.DeepEqual(got, want) {
		t.Fatalf("TargetFromContext = %v, %v; want %v, true", got, ok, want)
	}
}

func TestPeerTargetPassthrough(t *testing.T) {
	peerTarget := config.Target{BaseURL: "http://peer:8081", APIKey: "sk-virt-peer", Peer: "gateway-c"}
	m := New(map[string][]config.Entry{"dev": {
		{Target: peerTarget, Models: []config.Model{{ID: "f", Name: "f-real"}}},
		{Target: config.Target{BaseURL: "http://plain"}},
	}})
	route, ok := m.Select("Bearer dev")
	if !ok {
		t.Fatal("token not found")
	}
	// Model-matched resolution must carry the Peer marker.
	target, _, ok := route.Resolve("f")
	if !ok || target.Peer != "gateway-c" || target.BaseURL != "http://peer:8081" {
		t.Fatalf("Resolve lost peer marker: %+v", target)
	}
	// Round-robin (entry 0 is the peer) must too.
	if got := route.RoundRobin(); got.Peer != "gateway-c" {
		t.Fatalf("RoundRobin lost peer marker: %+v", got)
	}
	// Context handoff must preserve it end to end.
	ctx := WithTarget(t.Context(), target)
	got, ok := TargetFromContext(ctx)
	if !ok || got.Peer != "gateway-c" {
		t.Fatalf("context handoff lost peer marker: %+v", got)
	}
}

func TestConcurrentSelection(t *testing.T) {
	m := New(map[string][]config.Entry{"dev": {
		{Target: config.Target{BaseURL: "1"}},
		{Target: config.Target{BaseURL: "2"}},
		{Target: config.Target{BaseURL: "3"}},
	}})
	var counts [3]atomic.Int64
	var wg sync.WaitGroup
	for i := 0; i < 300; i++ {
		wg.Go(func() {
			route, ok := m.Select("Bearer dev")
			if !ok {
				t.Error("authentication failed")
				return
			}
			target := route.RoundRobin()
			counts[int(target.BaseURL[0]-'1')].Add(1)
		})
	}
	wg.Wait()
	for i := range counts {
		if counts[i].Load() != 100 {
			t.Fatal("unbalanced selection")
		}
	}
}
