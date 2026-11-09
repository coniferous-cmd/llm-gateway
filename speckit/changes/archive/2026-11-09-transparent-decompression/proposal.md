---
create-time: 2026-11-09 09:47:18
---

# Proposal

## Why

The gateway is the last HTTP hop before the client, but today it is fully
transparent about content coding: it forwards the caller's `Accept-Encoding`
upstream and passes whatever compressed bytes come back straight through,
including `Content-Encoding`, `Content-Length`, and the raw body. That makes
the gateway-to-client leg opaque in three ways:

- the caller can receive a compressed body even when it expected the gateway
  to be a plaintext boundary (and some SSE clients handle compressed streams
  poorly);
- conversation files capture the compressed bytes, so responses appear as
  base64 noise and decoding failures are invisible;
- the gateway cannot state a simple, testable invariant about what leaves its
  process.

The project has decided that the gateway-to-client direction is always
plaintext. Provider-side compression may still be used to save bandwidth on
the upstream leg, but the gateway must decode it before the response reaches
the client.

## What Changes

- **Always-on response decompression.** When an upstream response carries a
  single `Content-Encoding` of `gzip`, `deflate` (zlib framing), `br`, or
  `zstd`, the gateway SHALL stream-decode the body and SHALL remove
  `Content-Encoding`, `Content-Length`, and `Content-MD5` before sending the
  response. Streaming responses (SSE) SHALL be decoded incrementally, never
  buffered to completion.
- **Gateway-controlled upstream negotiation.** The gateway SHALL replace the
  caller's `Accept-Encoding` with the intersection of the caller's acceptable
  codings and the gateway-supported set `{gzip, deflate, br, zstd}`. An empty
  intersection removes the header. A caller that sends no `Accept-Encoding`
  causes no header to be added upstream. This guarantees the provider cannot
  select a coding the gateway cannot decode.
- **Explicit refusal instead of opaque passthrough.** If an upstream response
  nevertheless carries an unsupported coding or more than one coding, the
  gateway SHALL return `502 Bad Gateway` before writing any body byte, with a
  fixed diagnostic category. If decoding fails mid-stream, the response
  stream is aborted and the call is finalized as incomplete with a distinct
  category.
- **No-body responses are untouched.** `HEAD` and `204`/`205`/`304` responses
  carry no body; their headers are passed through unchanged.
- **Conversation records follow the client-visible payload.** When response
  bodies are decoded, capture observes the decoded bytes, so conversation
  files contain the plaintext the client actually received, and
  `response_complete` reflects decode failures. `observed_bytes` counts
  decoded bytes; completion relies on stream end rather than upstream
  `Content-Length`.
- **Documentation and parity record the divergence.** README, the parity
  checklist, and the two affected capability specs are updated to state the
  new plaintext invariant and to register this as a deliberate divergence
  from the Go reference implementation.

Assumptions recorded for this change:

- The behavior is always on; there is no configuration switch.
- Request bodies are not decoded. Compressed request bodies continue to pass
  through unchanged and continue to skip model inspection.
- No decompression expansion limit is enforced. Upstreams are
  operator-configured, so a decompression bomb is treated as an accepted
  residual risk.
- `Content-Encoding: deflate` is interpreted as RFC 9110 zlib-wrapped data;
  raw-deflate servers are not auto-detected.
- Multi-layer `Content-Encoding` is rejected rather than chained.

## Impact

- Affected specs: `virtual-key-proxy`, `conversation-file-logging`
- Affected code:
  - `src/proxy/forward.rs` — rewrite the upstream `Accept-Encoding`
  - `src/proxy/decode.rs` (new) — content-encoding classification, streaming
    decoders, response header rewriting
  - `src/server/mod.rs` — insert decoding before response observation, update
    completion accounting and error categories
  - `Cargo.toml` — add the compression dependency set
- Affected docs: `README.md`, `docs/parity-checklist.md`
- Build risk: the `zstd` decoder pulls in a C implementation (`zstd-sys`), so
  Windows CI and `cargo cross` builds must be verified.
