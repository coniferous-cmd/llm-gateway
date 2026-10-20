---
create-time: 2026-10-20 14:44:18
---

# Proposal

## Why

Locally running agents / applications need to reach multiple LLM providers (OpenAI, DeepSeek, local Ollama, etc.). Scattering each provider's real API key across caller configurations has two problems: changing a key or a provider means editing every caller, and a single key configuration cannot be shared and rotated across multiple places. We need a lightweight gateway installed on the local machine that authenticates inwardly with **virtual keys** and **forwards requests as-is** to the real providers, substituting the real key — fully decoupling callers from providers.

Core constraints (agreed during exploration):

- **Simple functionality, low resource usage**: local single-machine install, single binary, no database, no third-party runtime dependencies (YAML parsing excepted).
- **No protocol conversion, no token accounting, no validation, no hot reload**: the gateway is a transparent pipe that understands no API semantics; "unsupported cases" are forwarded as-is.

## What Changes

- New Go project skeleton: single binary `llm-gateway`, layered directory structure (`cmd/` + `internal/config|keymap|proxy|server`), dependencies pointing strictly downward.
- **Config-file driven**: a YAML config declares virtual keys and their target lists (`base_url` bare origin + `api_key`); the same virtual key may appear multiple times in the config, each occurrence adding a target. Loaded once at startup, no hot reload.
- **Virtual key authentication**: requests carry `Authorization: Bearer <virtual-key>`; a hit in the in-memory table proceeds, a miss returns 401. No other request validation whatsoever (no path, method, or Content-Type restrictions).
- **Round-robin routing**: when one virtual key has multiple targets, each request picks a target by round-robin (atomic counter modulo).
- **Transparent forwarding**: `base_url` + the original request path/query are joined into the target address; the HTTP `Host` is updated to the target authority; beyond that, only the `Authorization` header is replaced with the target's real `api_key` — request body and remaining headers are forwarded untouched; the provider's response (including SSE streams) is passed back as-is. When a target is unreachable or returns an error, the gateway relays the error as-is — no retry, no failover.
- **Readability-first layering**: each layer answers exactly one question (config = what the config looks like, keymap = which key / which target, proxy = how to forward, server = how the process is assembled), and `main()` only wires things up.

## Impact

- Affected specs: `virtual-key-proxy` (new capability: virtual key authentication and transparent forwarding)
- Affected code: brand-new codebase, no existing code affected. Target of zero runtime dependencies (`gopkg.in/yaml.v3` the sole expected exception, used only to parse the config).
