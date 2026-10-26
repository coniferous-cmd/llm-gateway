---
create-time: 2026-10-26 17:18:10
---

# Proposal

## Why

The gateway currently emits request summaries but does not preserve request and response bodies. Operators need to inspect the actual conversation exchanged during an individual API call, including streaming replies and failed calls, without requiring clients to supply a conversation identifier.

## What Changes

- Add opt-in conversation file logging: one JSON record per authenticated HTTP API call, organized into UTC date directories with unique gateway-generated request IDs.
- Capture the original client request body before model-name rewriting and the response body relayed to the client, including raw SSE events, with bounded capture and explicit truncation/completeness metadata.
- Record timing, request path, matched model, selected upstream, status, and sanitized failure category. Preserve normal forwarding, flushing, cancellation, and routing behavior.
- Add startup configuration for enablement, directory, and per-body capture size. Use private file permissions where supported, exclude headers and URL queries, and report persistence failures without failing calls.
- Document sensitive-body risks, bounded capture, failure behavior, and operator-managed retention. Automatic deletion, session grouping, protocol-specific reply reconstruction, and live crash recovery are outside this first version.

## Impact

- Affected specs: new `conversation-file-logging` capability; existing `virtual-key-proxy` forwarding behavior remains unchanged.
- Affected code: `internal/config`, a new `internal/conversationlog` package, `internal/server`, `internal/proxy` error observation, and `cmd/llm-gateway` startup wiring for both HTTP and encrypted listeners.
- Affected documentation: `config.example.yaml`, `README.md`; tests for configuration, capture, persistence, streaming, error handling, and concurrent calls.
- No new external service or dependency is required. Existing configurations remain valid and do not create conversation files unless explicitly enabled.
