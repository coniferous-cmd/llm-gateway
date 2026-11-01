---
create-time: 2026-10-20 17:47:55
---

## 1. Implementation

- [x] 1.1 Extend `internal/config`: `Model{ID, Name}`, `Entry{Target, Models}`, `Keys map[string][]Entry`; parse optional `keys[].models`, keep the field/line-number error style; validate non-empty `id`/`name` and per-token `id` uniqueness. Verify: table-driven tests cover a config with `models`, a legacy config without `models` (unchanged result), empty `id`/`name`, duplicate `id` within one token (and the same `id` under different tokens being allowed).
- [x] 1.2 Rework `internal/keymap`: `Select` returns a per-token `*Route` (entries, `id → entry` index, atomic counter) exposing `HasModels()`, `Resolve(id)`, `RoundRobin()`; model-matched picks must not advance the round-robin counter. Verify: tests cover hit/miss auth, resolve by id, unknown id miss, counter behavior (matched picks leave the rotation untouched), and `go test -race` stays green.
- [x] 1.3 Add the model body splicer in `internal/proxy/model.go` (pure byte functions, no HTTP): token-walk with `encoding/json.Decoder` + `InputOffset` to locate the top-level `model` **string** value, byte-splice `name` in place without re-serializing; `id == name` returns the input untouched. Verify: tests cover compact and pretty-printed JSON, escaped/unicode values, first of duplicate top-level `model` keys wins, nested `model` ignored, non-string `model`, malformed JSON and empty body reported as unresolvable.
- [x] 1.4 Add request preparation to `internal/proxy`: for model-declaring routes buffer the body (hard cap 32 MiB → `413`), skip inspection for non-identity `Content-Encoding`, resolve via `keymap`, rewrite when `id != name`, update `Request.ContentLength` and drop the stale `Content-Length` header; unresolvable bodies replay unchanged through round-robin. Verify: `httptest.Server` upstream asserts rewritten vs byte-identical bodies, correct `Content-Length`, 413 on oversized bodies, compressed and non-JSON bodies forwarded untouched.
- [x] 1.5 Wire it in `internal/server`: auth → route → model resolution; resolve miss returns `400 Bad Request` without contacting an upstream; request log gains `model` only when matched. Verify: server tests cover model-based entry selection across two entries of one token, 400 on unknown model id, `GET /v1/models` and model-less tokens keeping round-robin and untouched bodies, 401 unchanged.
- [x] 1.6 Extend `cmd/llm-gateway/main_test.go` integration: two mock upstreams behind one token with disjoint `models`, assert routing and `model` rewrite end-to-end plus the 400 path. Verify: `go test -count=1 -v ./cmd/llm-gateway`.
- [x] 1.7 Docs: `config.example.yaml` gains a `models` example (as in the request: `mimo-v2.6-flash` / `mimo-v2.6-pro`); `README.md` documents `models`, the (key, model id) routing rule, the `id`→`name` rewrite, the 400/413 behavior and the body-inspection boundary. Verify: a clean run-through following the README works.

## 2. Verification

- [x] 2.1 Check off each Scenario in `specs/virtual-key-proxy/spec.md`: models config + duplicate-id rejection, model selects the entry, unknown model → 400, no-model fallback, model-less token unchanged, alias rewrite, `id == name` byte-identical, plus the preserved round-robin / transparent-forwarding scenarios.
- [x] 2.2 `go test ./...`, `go vet ./...`, and `go test -race ./...` all green.
