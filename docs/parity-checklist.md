# Behavior parity with the Go reference

The Go implementation lives on `main`; this crate is a rewrite. The four
specs under `speckit/specs/` and the Go test suite are the acceptance
criteria. This file records how each area is covered in Rust and lists every
intentional divergence.

## Status

- `cargo test --all-targets`: **96 tests, 5 suites, all passing**
- `cargo clippy --all-targets -- -D warnings`: clean
- `cargo fmt --check`: clean

## Test mapping

| Go package / area | Go tests | Rust location | Count |
|---|---|---|---|
| `internal/config` parsing, validation, anchors, secrets | 17 | `src/config/` (inline `#[cfg(test)]`) | 23 |
| `internal/keymap` auth, round-robin, models, `[1m]`, expiry | 10 | `src/keymap.rs` | 7 |
| `internal/proxy` model scanner, splicing, path rewriting | 22 | `src/proxy/{model,rewrite,prepare}.rs` | 13 |
| `internal/server` pipeline, logging, conversation lifecycle | 23 | `src/server/mod.rs` + `tests/http_e2e.rs` | 4 + 5 |
| `internal/e2ee` handshake, framing, session | 16 | `src/e2ee/` + `tests/e2ee_e2e.rs` | 8 + 3 |
| `internal/daemon` pid file, stop, probe address | 10 | `src/daemon/mod.rs` + `tests/cli_binary.rs` | 4 + 5 |
| `internal/conversationlog` capture, store, collisions | 7 | `src/conversationlog/` | 9 |
| `cmd/llm-gateway` CLI flag matrix, routing | 23 | `src/cli/mod.rs` | 9 |
| Operational JSON logging | — | `src/logging.rs` + assertions in server tests | 2 |

Total: 96 tests covering the same behaviors as the 128 Go tests (Rust
combines several Go table rows into one parameterized test).

## Byte-level contracts verified

- **Path rewriting**: the full 40-case matrix — segment-boundary matching,
  first-match-wins (not longest), no chaining, `-` deletion marker, encoded
  `%2F`/`%25`/`%2f` handling, root source, trailing-slash literals, double
  slash, no path cleaning, empty query preserved.
- **Model routing**: only the first top-level `model` string counts; nested /
  numeric / null / object / array / malformed / non-JSON bodies are
  unresolvable and fall back to round-robin; splicing replaces exactly one
  JSON string literal and leaves every other byte (whitespace, escapes,
  unicode, number formats) untouched; the replacement uses Go's
  `encoding/json` HTML-safe escaping (`<`, `>`, `&`, U+2028/U+2029).
- **Forwarding**: request line and escaped path+query forwarded verbatim;
  `Host` rewritten to the target authority including port; `Authorization`
  replaced or deleted; hop-by-hop headers (including those named by
  `Connection`) removed; `TE: trailers` preserved like Go; no
  `X-Forwarded-For` added and existing values preserved; no `Accept-Encoding`
  added and no transparent decompression; status/headers/body passed through;
  429 relayed as-is; SSE flushed per chunk; no retry and no failover.
- **Body inspection**: 32 MiB limit; `Content-Encoding` other than `identity`
  (or empty) passes through uninspected; model-enabled tokens reject larger
  bodies with `413`; legacy tokens replay the buffered prefix plus the rest of
  the stream without rejecting; `Content-Length` removed only when the body
  was actually replaced.
- **Errors**: `401` with no upstream contact; `400` for unknown model;
  `413` for oversize; `502 Bad Gateway\n` for transport failure with the
  standard `text/plain; charset=utf-8` + `nosniff` headers.
- **Conversation files**: `<dir>/YYYY-MM-DD/HHMMSS_<32hex>.json` (UTC),
  directories 0700 and files 0600, published by `hard_link` from a
  `.conversation-*.tmp` sibling so an existing record is never overwritten,
  8 collision retries; `schema_version: 1`; field names, order and
  `omitempty` behavior match Go's struct tags (e.g. `upstream` and
  `model_id`/`model_name` omitted when no target was contacted); request
  captures the **pre-rewrite** body while the provider sees the rewritten one;
  `truncated`/`complete`/`observed_bytes`/`captured_bytes` semantics match;
  UTF-8 vs base64 encoding selection matches; `response_complete` overridden
  by the same error-category rule; diagnostics carry only a fixed
  `category` and never include paths, raw errors or bodies.
- **e2ee**: `Noise_IK_25519_ChaChaPoly_BLAKE2s` with prologue
  `llm-gateway-e2ee/1`; 2-byte big-endian length frames, 65535 max / 65519
  payload, zero-length frames rejected; 10 s handshake deadline; responder
  checks the initiator's static key against the allowlist **before** writing
  a reply; wrong pinned key aborts on both sides with no application data;
  per-TCP-connection handshake reused across HTTP keep-alive (verified: two
  requests over one session); base64 32-byte X25519 keys; `--keygen` prints
  exactly two lines.
- **CLI**: the 25-row flag matrix — exit codes 0/1/2, stdout vs stderr
  routing, legacy single-dash normalization (including `-config -version`
  consuming the value), precedence (`check-config` conflicts > `keygen` >
  `version` > `stop`+`daemon` conflict > `stop` > serve), subcommand flag
  scoping (`check-config` rejects `--daemon`/`--pid-file`/`--log-file`,
  `serve` rejects `--stop`, `stop`/`keygen` reject `--config`,
  `version extra` is an unknown command), `--log-file` silencing
  stdout/stderr, `completion bash` containing `bash completion`.
- **Daemon**: pid file `<pid>\n` at mode 0600; removal only when the file
  still records that pid; stale-pid cleanup; SIGTERM then 12 s then SIGKILL
  on Unix (immediate terminate on Windows); `waitReady` polling the pid file
  and a TCP probe for 5 s with a 250 ms connect timeout; `setsid` /
  `DETACHED_PROCESS|CREATE_NEW_PROCESS_GROUP`.
- **Lifecycle**: 10 s drain on shutdown, including an active SSE stream;
  connections are told to shut down gracefully so idle keep-alives close
  immediately instead of holding the drain window.
- **Logs**: one JSON line per request (including 401) with `method`, decoded
  `path` (query stripped), `target`, `status`, `duration` (ns), a nested
  `model{id,name}` only when matched, and `request_id` only when a call was
  captured; tokens never appear.

## Intentional divergences

1. **HTTP upgrades / hijack.** Go's `ReverseProxy` hijacks a connection for
   `101 Switching Protocols`. This rewrite has no hijack API, so a request
   with `Upgrade` or `CONNECT` is rejected with `502` and the record is
   marked `unsupported_hijack`. Streaming HTTP (SSE) is unaffected.
2. **Panics.** Go recovers a panicking handler, finalizes the conversation
   record, then re-panics. Rust has no `panic` recover equivalent across a
   future, so a panic ends the connection task without a finalized record.
   Malformed input must never panic (the config and proxy paths avoid
   `unwrap` on user data).
3. **Idle timeout.** Go's `IdleTimeout` (90 s) is not implemented; a
   connection with no requests stays open. Read-header (10 s), keep-alive,
   connect (10 s) and response-header (30 s) timeouts are implemented.
4. **Default `User-Agent`.** Go's `http.Transport` adds
   `User-Agent: Go-http-client/1.1` when the caller omits one. This rewrite
   sends no `User-Agent` in that case. A caller-supplied `User-Agent` is
   passed through unchanged either way. This avoids advertising a language
   the binary is not written in.
5. **YAML `<<` detection.** Go matches the `!!merge` tag; this rewrite
   matches the literal `<<` key. A quoted `"<<"` key is therefore treated as
   a merge rather than an unknown field. Every documented config is unaffected.
6. **TLS roots.** Go uses the OS trust store only. This rewrite uses the OS
   store and falls back to a bundled root set when the OS store is empty
   (common in minimal containers). This is strictly more capable.
7. **Conversation JSON escaping.** Go's `encoding/json` HTML-escapes
   `<`, `>` and `&` inside strings; `serde_json` does not. Both produce
   semantically identical JSON. (The request-body model splice *does* use
   Go's escaping, since that changes bytes sent to the provider.)
8. **Response completion with a known length.** Go's handler returns after
   `io.Copy` hits EOF. Hyper can stop polling a response once it has written
   a known `Content-Length`, so "captured bytes == expected length" is also
   treated as complete. A genuinely disconnected client is still recorded as
   `client_write`.
9. **Windows tests.** Unix-specific daemon tests (SIGTERM-then-SIGKILL, setsid)
   are `cfg(unix)`; the Windows equivalents are compiled and covered by the
   Windows CI runner. Local development on macOS covers the Unix paths.

## Not done

- **Differential harness against the Go binary.** The plan called for building
  the Go oracle from `main` in a `git worktree` and comparing `check-config`
  output, CLI stdout/stderr/exit codes, log field sets, and HTTP transcripts
  side by side. The Go toolchain in this environment is incomplete (`bufio`,
  `net/http/httputil` and `os/signal` are missing from `GOROOT`), so the
  oracle could not be built here. The suite above plus the binary smoke test
  cover the same behaviors; the harness is a cheap follow-up on a machine
  with a complete Go install:

  ```sh
  git worktree add /tmp/oracle main
  (cd /tmp/oracle && go build -trimpath -o oracle ./cmd/llm-gateway)
  ```

- **Cargo registry access.** `cargo build`/`test` in a sandbox that blocks
  crates.io needs `allowed_domains` for `index.crates.io`, `static.crates.io`
  and `crates.io`, or a pre-warmed cargo registry cache. The dependencies are
  pinned by the committed `Cargo.lock`, so a single `cargo fetch` is enough
  to make later builds offline.
