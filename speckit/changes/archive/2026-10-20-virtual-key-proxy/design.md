---
create-time: 2026-10-20 14:44:58
---

# Design

## Context

Brand-new codebase (the repo currently contains only `.claude/` and `speckit/`), so no existing-code constraints. The goal is an LLM gateway installed on the local machine: virtual keys authenticate inwardly, and requests are transparently forwarded outward to real providers. Hard constraints come from the exploration consensus — single binary, low resource usage, zero third-party runtime dependencies (YAML parsing excepted), and no accounting / conversion / validation / hot reload.

Key implementation facts (confirmed during exploration):

- Go's standard `net/http/httputil.ReverseProxy` natively supports SSE and streaming, but `FlushInterval = -1` must be set and an overall write timeout avoided (the default 30 s would truncate long answers).
- The single rewrite point for "transparency" is the `Authorization` header; URL joining happens in the `Director` function.
- Round-robin across multiple targets for the same token needs only an atomic counter — lock-free and testable.

## Decisions

- **Layered structure, dependencies strictly downward**: `cmd/llm-gateway` (assembly) → `internal/server` (HTTP lifecycle, logging, 401) → `internal/proxy` (ReverseProxy assembly, header swap, URL join) → `internal/keymap` (auth lookup + round-robin) → `internal/config` (YAML parsing and validation). One package per layer, one file to start; each layer is readable and testable on its own.
- **Config format** (`gopkg.in/yaml.v3`, the only third-party dependency):
  ```yaml
  listen: "127.0.0.1:8080"
  keys:
    - token: sk-virt-dev
      target:
        base_url: https://api.openai.com   # bare origin, no path
        api_key: sk-real-xxx
    - token: sk-virt-dev                   # same token again → appended as a second target
      target:
        base_url: http://127.0.0.1:11434
        api_key: ollama
  ```
- **Parsed into `map[token][]Target` at startup**: the config layer merges same-token entries, trims trailing slashes from base_url, and validates required fields (token/base_url non-empty, URL parseable); on error the process exits and prints the specific line-level reason.
- **Round-robin**: the keymap layer holds one `atomic.Uint64` counter per token and routes via `targets[counter % len]`; with a single target the result is constant.
- **Auth placement**: server-layer middleware extracts the Bearer token → queries the keymap; a miss writes 401. On a hit, the selected `Target` is put into the request context and passed to the proxy layer.
- **Forwarding**: the proxy layer builds (or reuses) a `ReverseProxy` per request — the `Director` sets `req.URL.Scheme/Host` and the HTTP `req.Host` (both from target.base_url; Host includes the port, for upstream virtual-host routing), keeps the original `req.URL.Path` (and `RawPath`) + `RawQuery`, and replaces the `Authorization` header; remaining application-layer headers are preserved and ReverseProxy is prevented from auto-appending `X-Forwarded-For` (any caller-supplied value is kept), while standard HTTP hop-by-hop header handling is allowed; `FlushInterval = -1`; the `Transport` is a tuned `http.Transport` (sensible MaxIdleConnsPerHost, **no** overall timeout, relying on connect / response-header timeouts); the `ErrorHandler` uniformly returns a plain-text 502 (upstream unreachable, no retry).
- **No hot reload**: the config is read once at startup; changing it = restarting the process. No file watching, no SIGHUP handling.
- **Minimal observability**: `log/slog` to stdout — one line per request (method, path, selected target, status code, duration); no metrics endpoint.
- **main() only assembles**: parse the flag (`-config` path) → config.Load → keymap.New → server.New → start and handle graceful shutdown on SIGINT/SIGTERM; target ≤ 50 lines.

## Risks / Trade-offs

- **Risk: ReverseProxy's default behavior truncates streaming responses** — mitigation: `FlushInterval = -1` plus no overall WriteTimeout; the implementation tasks include a manual SSE scenario.
- **Risk: no failover means callers get a 502 directly when a single target fails** — this is an accepted scope decision (errors relayed as-is, semantics left to the caller); the value of multiple targets is load spreading, not fault tolerance.
- **Trade-off: real keys stored in plaintext in the config file** — acceptable for a local single-machine, single-administrator setup; buys zero storage and zero key-management complexity. No secret mechanism beyond environment-variable overrides.
- **Trade-off: round-robin across providers with different capabilities may cause answer-quality variance** — explicitly chosen as option A by the user; the person writing the config is responsible for putting only semantically equivalent / compatible endpoints under the same token.
- **Trade-off: no request validation at all** — the gateway could be used as a stepping stone for an open proxy; mitigated by listening on `127.0.0.1` only (the default `listen` value).
