package keymap

import (
	"reflect"
	"testing"

	"llm-gateway/internal/config"
)

func TestSelectedTargetRewrite(t *testing.T) {
	cfg, err := config.Parse([]byte(`keys:
  - token: dev
    target:
      base_url: https://one.example
      rewrite: ['/api:/v1']
    models: [{id: one, name: real-one}]
  - token: dev
    target:
      base_url: https://two.example
      rewrite: ['/api:/v2']
    models: [{id: two, name: real-two}]
`))
	if err != nil {
		t.Fatal(err)
	}
	m := New(cfg.Keys)
	// Mutating the input must not affect the route's rule snapshots.
	cfg.Keys["dev"][0].Target.Rewrite[0].Destination = "/mutated"
	cfg.Keys["dev"][1].Target.Rewrite[0].Source = "/wrong"
	route, _ := m.Select("Bearer dev")
	for _, id := range []string{"two", "one", "two"} {
		target, _, ok := route.Resolve(id)
		want := "/v1"
		if id == "two" {
			want = "/v2"
		}
		if !ok || !reflect.DeepEqual(target.Rewrite, []config.RewriteRule{{Source: "/api", Destination: want}}) {
			t.Fatalf("Resolve(%q) = %+v, %v", id, target, ok)
		}
	}
	route.Resolve("unknown")
	for _, want := range []string{"/v1", "/v2", "/v1", "/v2"} {
		target := route.RoundRobin()
		if !reflect.DeepEqual(target.Rewrite, []config.RewriteRule{{Source: "/api", Destination: want}}) {
			t.Fatalf("RoundRobin = %+v, want %s (model matches must not advance rotation)", target, want)
		}
	}
}
