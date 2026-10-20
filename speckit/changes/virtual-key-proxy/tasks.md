---
create-time: 2026-10-20 14:46:40
---

## 1. Implementation

- [x] 1.1 Initialize the Go module and directory skeleton: `go mod init`, `cmd/llm-gateway/`, empty packages under `internal/{config,keymap,proxy,server}`; add the `gopkg.in/yaml.v3` dependency and `config.example.yaml`. Verify: `go build ./...` passes.
- [x] 1.2 Implement `internal/config`: YAML loading, required-field validation, merging same-token entries into `map[token][]Target`, and trailing-slash normalization of base_url. Verify: table-driven unit tests pass (valid config, missing fields, invalid URL, multiple entries for one token, both trailing-slash spellings).
- [ ] 1.3 Implement `internal/keymap`: auth lookup (Bearer extraction / miss → 401 semantics) and per-token atomic-counter round-robin routing. Verify: unit tests cover hit/miss, constant routing with a single target, 6 consecutive requests over 3 targets rotating 1,2,3,1,2,3, and race-free concurrent routing (`go test -race`).
- [x] 1.4 Implement `internal/proxy`: `ReverseProxy` assembly — the `Director` joins `base_url + original path/query` and updates the HTTP `Host` to the target authority, replacing only the `Authorization` header beyond that; `FlushInterval = -1`; `ErrorHandler` returns 502. Verify: unit tests use `httptest.Server` as the upstream and assert the URL the upstream receives, that `Authorization` is swapped, and that the original body / remaining headers are unchanged; when the upstream returns 429 the caller receives 429 with the original body; when the upstream is unreachable the caller receives 502.
- [x] 1.5 Implement `internal/server` and `cmd/llm-gateway/main.go`: middleware (auth → routing → inject target into context), one `slog` line per request, the `-config` flag, graceful shutdown on SIGINT/SIGTERM, default listen on `127.0.0.1:8080`. Verify: `go vet ./...` and all unit tests pass; main stays purely assembly.
- [x] 1.6 End-to-end manual test (real local chain): start the gateway with Ollama (or a local mock upstream) as the target and use `curl` with a virtual key against `/v1/chat/completions` — assert: normal forwarding and response passthrough; 401 on a bad token; any non-`/v1` path is also forwarded as-is; with 2 targets on the same token requests alternate between them; SSE streaming arrives chunk by chunk (not buffered whole).
- [x] 1.7 Wrap-up: `README.md` (install, config format, running, curl examples), `.gitignore`, confirm `go build` produces a single static binary. Verify: a clean run-through following the README works.

## 2. Verification

- [x] 2.1 Check off each Scenario in `specs/virtual-key-proxy/spec.md`: 401, same token with multiple targets, round-robin sequence, URL joining, Authorization-only swap, SSE passthrough, 502 without retry, 429 relayed as-is.
- [ ] 2.2 `go test -race ./...` all green; `go vet ./...` no warnings.
