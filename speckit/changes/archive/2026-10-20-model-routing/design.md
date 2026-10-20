---
create-time: 2026-10-20 17:45:17
---

# Design

## Context

The gateway today is a byte-preserving pipe: `internal/config` parses the YAML into `Config{Listen, Keys map[string][]Target}`, `internal/keymap` authenticates the Bearer token and round-robins one `config.Target` out of the token's list, `internal/proxy` swaps scheme/host/`Authorization` via `httputil.ReverseProxy.Director`, `internal/server` wires auth + logging + lifecycle. Request bodies are never read; requests stream through.

This change adds `keys[].models` (`id`/`name` pairs) and routes by (virtual key, model id), rewriting the forwarded `model` name. That requires reading request bodies for the first time — the one deliberate relaxation of the transparency principle.

## Decisions

- **Config shape** (backward compatible): `models` is optional per key entry, sibling of `target`.

  ```yaml
  keys:
    - token: sk-...
      target: {base_url: ..., api_key: ...}
      models:
        - id: mimo-v2.6-flash    # what callers send
          name: mimo-v2.6-flash  # what the provider receives
  ```

  Internal shape becomes `config.Entry{Target, Models []Model}` with `Model{ID, Name string}` and `Config.Keys map[string][]Entry`; `keymap` is the only consumer, so the ripple stops there. Validation (same field/line-number error style as today): `id` and `name` required non-empty; `id` unique within one token; duplicate `id` is a startup error.

- **Selection stays in `keymap`, body work stays in `proxy`** (layering unchanged: keymap answers "which entry", proxy answers "how to forward"). `keymap.Map.Select(authz)` returns a `*keymap.Route` (per-token: entries, `id → entry index`, round-robin counter). The route exposes `HasModels()`, `Resolve(id) (config.Target, bool)`, `RoundRobin() config.Target`. `proxy` gains a request-preparation step (new file `internal/proxy/model.go`) that runs before the `Director`: inspect body → resolve → possibly rewrite → hand the target to the `Director` via the existing context value. `server` keeps auth (401), logging and lifecycle, and turns a resolve miss into `400 Bad Request`.

- **Routing semantics**:
  - Token declares no `models` on any entry → byte-for-byte today's behavior: body never read, round-robin.
  - Token declares `models` and the body is a JSON object with a top-level `model` **string** → match against the token's `id`s; hit forwards to that entry's target without advancing the round-robin counter; miss returns `400`, no upstream contacted.
  - No resolvable model id (no body, `GET /v1/models`, non-JSON, non-string `model`, compressed body) → round-robin across all entries, body untouched.

- **Extraction and rewrite are byte-splices, not re-serialization.** Unmarshal → marshal would reorder keys and reformat numbers (silently changing bodies). Instead: `encoding/json.Decoder` token-walks to the top-level `model` key, `Decoder.InputOffset()` brackets the exact value bytes, and only that span is replaced with `json.Marshal(name)`. Everything else stays byte-identical. If `id == name` the body is not rebuilt at all. A non-string or nested `model` is not "resolvable" and falls through to round-robin.

- **Body buffering policy**: inspection happens only for model-declaring tokens, where the model id must be found. The body is buffered up to a fixed hard cap (32 MiB; a local gateway serving its own callers); over the cap returns `413 Request Entity Too Large`. Compressed bodies (`Content-Encoding` other than identity) are never decompressed — treated as unresolvable, forwarded untouched. On rewrite, `Request.ContentLength` is updated and the stale `Content-Length` header removed so the transport re-frames the request.

- **The 400 has a small plain-text body** (`http.Error`, matching the existing 401/502 style), and the request log line gains a `model` field only when a model was matched (kept out of the spec on purpose; a trivial observability nicety).

## Risks / Trade-offs

- **Transparency principle relaxed**: the gateway now reads bodies. Mitigation: exactly one top-level JSON field is ever read or written; the spec pins the boundary (no other field, no response touching, no protocol semantics), and tokens without `models` keep full streaming transparency.
- **Buffering cost on model-enabled routes**: up to 32 MiB per in-flight request instead of streaming. Mitigation: cap + `413`, and the cap only applies where model routing is configured.
- **Wrong routing on unresolvable bodies**: a model-enabled token receiving a body whose `model` we cannot read falls back to round-robin, which may hit an entry that does not serve the caller's intent. Mitigation: documented; the alternative (rejecting anything unparseable) would break `GET /v1/models` and streaming uploads, so fallback is the lesser evil.
- **Byte-splice correctness**: `InputOffset` bracketing must handle whitespace and escaped strings. Mitigation: focused unit tests on the splicer (pretty-printed JSON, unicode escapes, duplicate `model` keys — first top-level occurrence wins, nested `model` ignored).
- **Duplicate-`id` ban within one token** is the simple disambiguation rule; serving the same model from two entries of one token would need a tie-break (round-robin among matches) — explicitly out of scope, rejected at startup instead.
