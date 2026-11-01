---
create-time: 2026-10-20 17:43:36
---

# Proposal

## Why

Callers increasingly talk to providers that host several models behind one endpoint (e.g. Xiaomi's `mimo-v2.6-flash` / `mimo-v2.6-pro`). Today the gateway routes only by virtual key: a key entry owns exactly one target and every request under that key is forwarded untouched. That leaves two gaps:

1. **One key cannot steer different models to different providers.** Selecting an upstream by model id — "this key's `flash` traffic goes to provider A, its `pro` traffic to provider B" — is impossible; the caller would need a different virtual key per provider.
2. **The caller is stuck with the provider's model names.** Renaming a model on the provider side forces every caller to reconfigure; there is no place to say "callers write `mimo-v2.6-flash`, the upstream should hear `<the real name>`".

A `models` mapping per key entry closes both gaps: the model id in the request becomes part of the routing key, and the configured `name` is what the provider receives.

## What Changes

- **Config gains `keys[].models`**: an optional list of `{id, name}` pairs on each key entry, alongside `target`. `id` is the model name callers use, `name` is the model name forwarded to the provider. Existing configs without `models` keep working unchanged.
- **Routing key becomes (virtual key, model id)**: for a JSON request whose body carries a `model` field, the gateway picks the key entry whose `models` list contains a matching `id` and forwards to that entry's target. A key whose entries declare no `models` keeps today's round-robin behavior.
- **Model name rewrite on forward**: when a matching model declares `id != name`, the gateway rewrites the request body's `model` value from `id` to `name` before forwarding. This is the first place the gateway inspects a request body — accepted cost for model-level control.
- **Unknown model id is rejected**: a body `model` value that matches no `id` under the authenticated key returns `400 Bad Request` without contacting any provider.
- **Requests without a resolvable `model`** (non-JSON bodies, GET requests, paths such as `GET /v1/models`) are forwarded as before: round-robin across the key's entries, body untouched.

## Impact

- Affected specs: `virtual-key-proxy` (delta: configuration schema, routing, transparent forwarding)
- Affected code: `internal/config` (parse and validate `models`), `internal/keymap` (entry selection by model id), `internal/proxy` (body inspection and `model` rewrite), `internal/server` (wire model lookup into the request path), `config.example.yaml` and `README.md` (document the new field). Tests updated across `config`, `keymap`, `proxy`, `server`, and `cmd/llm-gateway`.
- Design tradeoff: the "transparent pipe that parses nothing" principle is deliberately relaxed — only the `model` field of a JSON object body is read (and possibly rewritten); all other fields and headers still pass through untouched, and no protocol semantics beyond that single field are understood.
