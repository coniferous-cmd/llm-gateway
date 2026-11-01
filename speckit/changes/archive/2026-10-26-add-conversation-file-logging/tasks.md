---
create-time: 2026-10-26 17:21:00
---

## 1. Configuration and record primitives

- [x] 1.1 Add startup-only `conversation_log` configuration with disabled default, config-relative directory resolution, and a positive per-body limit defaulting to 8388608. Test omission, explicit enable/disable, defaults, absolute/relative paths, blank directories, wrong types, and zero/negative limits without touching the filesystem during parsing.
- [x] 1.2 Add `internal/conversationlog` versioned record types, injectable clock/ID generation, and concurrency-safe bounded request/response capture primitives. Test byte limits, exact-limit bodies, empty bodies, unknown lengths, partial reads/writes, observed counts, truncation versus completeness, and UTF-8/base64 lossless encoding including a truncated multibyte character.

## 2. File persistence

- [x] 2.1 Implement UTC date/time filenames, exclusive private temporary files, complete serialization before non-overwriting final publication, and temporary cleanup on ordinary errors. Verify valid JSON, concurrent unique files, forced filename collisions, permission modes on POSIX, and no payload-derived filenames.
- [x] 2.2 Add injectable filesystem-failure tests for initialization, write, close, and publication failures. Confirm safe operational diagnostics omit bodies, credentials, raw query strings, and raw transport errors, and that later persistence attempts can recover when storage becomes available.

## 3. Request lifecycle integration

- [x] 3.1 Extend server construction compatibly with an optional shared recorder; start capture only after authentication and before `proxy.Prepare`. Add request IDs to existing request summaries. Test no capture/storage when disabled, no unauthorized conversation files, original alias capture, and one record for preparation failures.
- [x] 3.2 Extend the response writer and add per-request proxy observation for upstream EOF/read failures, transport failures, partial client writes, and cancellation without altering status, headers, Flush/Unwrap/Hijack, or generic 502 behavior. Verify complete HTTP error responses remain complete and unsupported hijacks are not claimed as captured conversations.
- [x] 3.3 Finalize once on success, rejection, cancellation, and panic/ReverseProxy abort, preserving original panic behavior and recording only committed statuses. Test partial records for interrupted responses and unread request bodies without draining bodies solely for logging.
- [x] 3.4 Initialize the recorder in `cmd/llm-gateway` and wire it to both ordinary and encrypted listeners. Verify unavailable storage does not prevent serving, `--log-file` remains independent, relative paths remain config-relative in daemon mode, and graceful shutdown includes active finalization. Coordinate both listener lifecycles before process exit; verify active encrypted streaming survives idle ordinary-listener shutdown and ordinary startup failure cancels and drains the peer lifecycle.

## 4. Integration verification and documentation

- [x] 4.1 Add end-to-end tests for JSON success, raw SSE (assert first flushed chunk arrives before completion), upstream 429, connection refusal, truncated upstream response, and client cancellation. Compare forwarded bodies/status/headers with logging disabled and enabled; verify no added request headers or query leakage.
- [x] 4.2 Test binary/compressed bodies, path/model rewrites, capture-limit overflow without new HTTP rejections, concurrency isolation, and decrypted-peer logging parity. Run race tests on supported environments and ensure snapshots do not race transport read/close activity.
- [x] 4.3 Document enablement and a representative JSON record in `README.md` and `config.example.yaml`. Explain config-relative paths, 8 MiB per-body default, raw SSE/base64, completion/truncation flags, unredacted sensitive bodies, POSIX permissions/Windows ACLs, plaintext-at-rest peer records, manual retention, and crash/slow-storage limitations.
- [x] 4.4 Run `go test ./...`, `go vet ./...`, and `go test -race ./...` where supported; record platform restrictions rather than claiming unavailable checks passed. Validate the change artifacts and review all acceptance scenarios against tests before marking implementation complete.
