package proxy

import (
	"errors"
	"io"
	"net/http"
	"net/http/httptest"
	"strconv"
	"strings"
	"testing"

	"llm-gateway/internal/config"
	"llm-gateway/internal/keymap"
)

func routeFor(t *testing.T, entries ...config.Entry) *keymap.Route {
	t.Helper()
	m := keymap.New(map[string][]config.Entry{"virtual": entries})
	route, ok := m.Select("Bearer virtual")
	if !ok {
		t.Fatal("route missing")
	}
	return route
}

func TestPrepareModelRouting(t *testing.T) {
	entries := []config.Entry{
		{Target: config.Target{BaseURL: "flash"}, Models: []config.Model{{ID: "alias", Name: "real-model"}, {ID: "same", Name: "same"}}},
		{Target: config.Target{BaseURL: "pro"}, Models: []config.Model{{ID: "p", Name: "pro-real"}}},
	}
	tests := []struct {
		name, body, target, model, wantBody string
		wantErr                             error
	}{
		{"alias rewrite", `{"model":"alias","n":1}`, "flash", "alias", `{"model":"real-model","n":1}`, nil},
		{"id equals name", "{\n  \"model\": \"same\",\n  \"n\": 1\n}", "flash", "same", "{\n  \"model\": \"same\",\n  \"n\": 1\n}", nil},
		{"second entry", `{"model":"p","n":1}`, "pro", "p", `{"model":"pro-real","n":1}`, nil},
		{"unknown model id", `{"model":"nope"}`, "", "", "", ErrUnknownModel},
	}
	for _, tt := range tests {
		t.Run(tt.name, func(t *testing.T) {
			req := httptest.NewRequest("POST", "/v1/chat/completions", strings.NewReader(tt.body))
			target, model, err := Prepare(req, routeFor(t, entries...))
			if tt.wantErr != nil {
				if !errors.Is(err, tt.wantErr) {
					t.Fatalf("error = %v, want %v", err, tt.wantErr)
				}
				return
			}
			if err != nil {
				t.Fatal(err)
			}
			if target.BaseURL != tt.target || model.ID != tt.model {
				t.Fatalf("routed to %q with model id %q, want %q %q", target.BaseURL, model.ID, tt.target, tt.model)
			}
			body, _ := io.ReadAll(req.Body)
			if string(body) != tt.wantBody {
				t.Fatalf("body = %q, want %q", body, tt.wantBody)
			}
			if req.ContentLength != int64(len(body)) {
				t.Fatalf("ContentLength = %d, want %d", req.ContentLength, len(body))
			}
		})
	}
}

func TestPrepareUnresolvableBodiesFallBackToRoundRobin(t *testing.T) {
	entries := []config.Entry{
		{Target: config.Target{BaseURL: "first"}, Models: []config.Model{{ID: "m", Name: "m"}}},
		{Target: config.Target{BaseURL: "second"}},
	}
	route := routeFor(t, entries...)
	for i, body := range []string{"opaque body", `{"n":1}`, `[{"model":"m"}]`, `{"model":123}`} {
		req := httptest.NewRequest("POST", "/v1/chat/completions", strings.NewReader(body))
		target, model, err := Prepare(req, route)
		want := []string{"first", "second"}[i%2]
		if err != nil || model.ID != "" || target.BaseURL != want {
			t.Fatalf("%q routed to %q with model id %q, %v; want %q", body, target.BaseURL, model.ID, err, want)
		}
		got, _ := io.ReadAll(req.Body)
		if string(got) != body {
			t.Fatalf("body changed: %q, want %q", got, body)
		}
	}
	// A model match must not shift the rotation: four picks later the counter
	// is back at the first entry.
	req := httptest.NewRequest("POST", "/v1/chat/completions", strings.NewReader(`{"model":"m"}`))
	if target, _, err := Prepare(req, route); err != nil || target.BaseURL != "first" {
		t.Fatal(target, err)
	}
	req = httptest.NewRequest("GET", "/v1/models", nil)
	if target, _, err := Prepare(req, route); err != nil || target.BaseURL != "first" {
		t.Fatal(target, err)
	}
}

func TestPrepareLegacyTokenLogsModelWithoutRewriting(t *testing.T) {
	body := `{"model":"alias"}`
	req := httptest.NewRequest("POST", "/v1/chat/completions", strings.NewReader(body))
	target, model, err := Prepare(req, routeFor(t, config.Entry{Target: config.Target{BaseURL: "legacy"}}))
	if err != nil || target.BaseURL != "legacy" || model.ID != "alias" || model.Name != "alias" {
		t.Fatal(target, model, err)
	}
	got, _ := io.ReadAll(req.Body)
	if string(got) != body {
		t.Fatalf("body changed: %q", got)
	}
}

func TestPrepareLegacyLargeBodyPassesThrough(t *testing.T) {
	body := `{"model":"alias","data":"` + strings.Repeat("x", maxInspectedBody) + `"}`
	for _, unknownLength := range []bool{false, true} {
		req := httptest.NewRequest("POST", "/v1/chat/completions", strings.NewReader(body))
		if unknownLength {
			req.ContentLength = -1
		}
		target, model, err := Prepare(req, routeFor(t, config.Entry{Target: config.Target{BaseURL: "legacy"}}))
		if err != nil || target.BaseURL != "legacy" || model.ID != "" {
			t.Fatal(target, model, err)
		}
		got, err := io.ReadAll(req.Body)
		if err != nil || string(got) != body {
			t.Fatalf("large body changed (unknown length %v): %v", unknownLength, err)
		}
	}
}

func TestPrepareCompressedBodyPassesThrough(t *testing.T) {
	body := "\x1f\x8b{\"model\":\"alias\"}"
	req := httptest.NewRequest("POST", "/v1/chat/completions", strings.NewReader(body))
	req.Header.Set("Content-Encoding", "gzip")
	target, _, err := Prepare(req, routeFor(t, config.Entry{Target: config.Target{BaseURL: "any"}, Models: []config.Model{{ID: "alias", Name: "real"}}}))
	if err != nil || target.BaseURL != "any" {
		t.Fatal(target, err)
	}
	got, _ := io.ReadAll(req.Body)
	if string(got) != body {
		t.Fatalf("compressed body changed: %q", got)
	}
}

// A body that dies mid-read must be replayed untouched (partial bytes plus
// the original closer) and routed round-robin rather than rejected.
func TestPrepareUnreadableBodyFallsBackToRoundRobin(t *testing.T) {
	entries := []config.Entry{
		{Target: config.Target{BaseURL: "first"}, Models: []config.Model{{ID: "m", Name: "m"}}},
		{Target: config.Target{BaseURL: "second"}, Models: []config.Model{{ID: "m", Name: "m"}}},
	}
	req := httptest.NewRequest("POST", "/v1/chat/completions", nil)
	req.Body = &flakyBody{prefix: []byte(`{"model":"ali`)}
	req.ContentLength = int64(len(`{"model":"ali`) + 10)
	target, model, err := Prepare(req, routeFor(t, entries...))
	if err != nil || model.ID != "" || target.BaseURL != "first" {
		t.Fatalf("routed to %q with model id %q, %v; want first, \"\", nil", target.BaseURL, model.ID, err)
	}
	data, _ := io.ReadAll(req.Body)
	if string(data) != `{"model":"ali` {
		t.Fatalf("replayed body = %q, want the partial prefix", data)
	}
}

// flakyBody serves a fixed prefix once and then fails every read.
type flakyBody struct{ prefix []byte }

func (f *flakyBody) Read(p []byte) (int, error) {
	if len(f.prefix) == 0 {
		return 0, errors.New("connection reset mid-body")
	}
	n := copy(p, f.prefix)
	f.prefix = f.prefix[n:]
	return n, nil
}

func (f *flakyBody) Close() error { return nil }

func TestPrepareBodyTooLarge(t *testing.T) {
	body := strings.Repeat("x", maxInspectedBody+1)
	req := httptest.NewRequest("POST", "/v1/chat/completions", strings.NewReader(body))
	_, _, err := Prepare(req, routeFor(t, config.Entry{Target: config.Target{BaseURL: "any"}, Models: []config.Model{{ID: "m", Name: "m"}}}))
	if !errors.Is(err, ErrBodyTooLarge) {
		t.Fatalf("error = %v, want %v", err, ErrBodyTooLarge)
	}
}

// End-to-end through the reverse proxy: the upstream must see the rewritten
// body and a matching Content-Length.
func TestPrepareForwarding(t *testing.T) {
	var gotBody, gotLength string
	upstream := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		data, _ := io.ReadAll(r.Body)
		gotBody, gotLength = string(data), r.Header.Get("Content-Length")
		w.WriteHeader(http.StatusNoContent)
	}))
	defer upstream.Close()
	p := New(nil)
	m := keymap.New(map[string][]config.Entry{"virtual": {{
		Target: config.Target{BaseURL: upstream.URL, APIKey: "real"},
		Models: []config.Model{{ID: "alias", Name: "real-model"}},
	}}})
	gateway := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		route, _ := m.Select(r.Header.Get("Authorization"))
		target, _, err := Prepare(r, route)
		if err != nil {
			http.Error(w, "Bad Request", http.StatusBadRequest)
			return
		}
		p.ServeHTTP(w, r.WithContext(keymap.WithTarget(r.Context(), target)))
	}))
	defer gateway.Close()
	body := `{"model":"alias","messages":[{"role":"user","content":"hi"}]}`
	req, _ := http.NewRequest("POST", gateway.URL+"/v1/chat/completions", strings.NewReader(body))
	req.Header.Set("Authorization", "Bearer virtual")
	resp, err := http.DefaultClient.Do(req)
	if err != nil {
		t.Fatal(err)
	}
	resp.Body.Close()
	want := `{"model":"real-model","messages":[{"role":"user","content":"hi"}]}`
	if resp.StatusCode != http.StatusNoContent || gotBody != want {
		t.Fatalf("upstream got %q with status %d, want %q", gotBody, resp.StatusCode, want)
	}
	if gotLength != strconv.Itoa(len(want)) {
		t.Fatalf("Content-Length = %q, want %d", gotLength, len(want))
	}
}
