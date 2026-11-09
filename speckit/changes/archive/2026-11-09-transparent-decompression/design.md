---
create-time: 2026-11-09 09:48:30
---

# Design

## Context

Today the proxy is byte-transparent. `Forwarder::send` (`src/proxy/forward.rs`)
clones the caller's headers, removes hop-by-hop headers, rewrites `Host` and
`Authorization`, and forwards. On the response path it removes hop-by-hop
headers and returns the upstream parts and `Incoming` body untouched
(`src/server/mod.rs:303-310`). `ObservedBody` therefore captures the encoded
bytes, `Content-Length` is trusted for completion accounting, and there is no
compression dependency in `Cargo.toml`.

The change introduces one gateway-controlled step on each direction of the
connection:

```
request headers: caller Accept-Encoding ──filter──▶ upstream Accept-Encoding
response body:   provider coded bytes    ──decode──▶ caller plaintext bytes
```

Everything else about forwarding remains as it is.

## Decisions

### 1. The gateway owns response negotiation

`Forwarder::send` replaces the outgoing `Accept-Encoding` with the
intersection of the caller's acceptable codings and
`{gzip, deflate, br, zstd}`. Parsing rules:

- Collect every `Accept-Encoding` field line; split each on `,`.
- Parse `coding[;q=value]`; trim whitespace and compare codings
  case-insensitively. A missing `q` is `q=1`; `q=0` is not acceptable.
- `*` stands for the supported set, but an explicit entry for a coding
  overrides the wildcard for that coding (RFC 9110 semantics), so
  `*;q=1, gzip;q=0` yields `deflate, br, zstd`.
- Emit the supported codings in the fixed order `gzip, deflate, br, zstd`
  without q parameters. An empty result removes the header; a request that
  carried no `Accept-Encoding` gets none added.

This is required for the correctness of the invariant, not just an
optimization: without it a provider could select a coding the gateway cannot
decode. Because the gateway only asks upstream for compression when the
caller asked for it, streaming latency and upstream bandwidth keep their
current shape.

### 2. Decoding is streaming and sits before response observation

Add `src/proxy/decode.rs` with:

- `Coding` enum `{ Gzip, Deflate, Br, Zstd }`.
- `classify(&HeaderMap) -> Result<Option<Coding>, UnsupportedEncoding>`:
  join all `Content-Encoding` field lines, split on `,`, trim and lowercase.
  Zero tokens or a single `identity` means no coding; exactly one supported
  token selects a decoder; anything else is unsupported.
- `BodyBufReader`: implements `tokio::io::AsyncBufRead` over the upstream
  `ReqBody`. It pulls data frames on demand, skips trailer frames, and
  distinguishes "no buffered bytes yet" (`Poll::Pending`) from upstream EOF.
  This is the critical property: a synchronous `Read`-based decoder would
  mistake an empty buffer for end of stream and break incremental SSE
  decoding.
- Decoder construction with `async-compression` 0.4:
  `tokio::bufread::GzipDecoder`, `ZlibDecoder` (for `Content-Encoding:
  deflate`, which RFC 9110 defines as zlib-wrapped DEFLATE),
  `BrotliDecoder`, `ZstdDecoder`, boxed as `Box<dyn AsyncRead + Unpin>`.
- `ReadBody`: a `Body` adapter that polls the decoder into a `BytesMut`,
  emits `Frame::data`, and ends on EOF. On a decoder error it returns a
  `DecodeError` so the observer can classify it.

`server::handle` classifies the upstream response after `into_parts()`:

- no coding / `identity`: existing path unchanged.
- supported single coding: remove `Content-Encoding`, `Content-Length`, and
  `Content-MD5`, wrap the body in `ReadBody`, then in `ObservedBody`.
- unsupported or multi-layer coding: finalize the call with a local
  `502 Bad Gateway` and category `upstream_encoding`; the upstream body is
  never read or forwarded.
- bodyless responses (`HEAD`, `204`, `205`, `304`): pass headers and body
  through untouched.
- empty body with a coding: if `Incoming::is_end_stream()` is already true,
  strip the coding-derived headers and use an empty body; otherwise
  `ReadBody` treats "decoder error before any input byte and before any
  output" as a clean empty stream.

Because decoding happens inside the observation point, conversation capture
records the plaintext the caller received, and a decode error is observed as
a body error rather than a completed response.

### 3. Completion accounting and diagnostics

For decoded responses `RequestState::set_expected_response_bytes(None)` is
used; completion comes from stream EOF. `ObservedBody` gains error
classification: a boxed `DecodeError` maps to the fixed category
`upstream_decode`, any other body error keeps `upstream_read`. The synthesized
502 uses `upstream_encoding`. Neither category carries raw error text, which
preserves the existing diagnostics rule.

### 4. Header handling details

- `Content-Encoding`, `Content-Length`, and `Content-MD5` are removed from
  decoded responses. HTTP/1.1 framing falls back to chunked.
- `ETag` and `Vary` are left in place. A strong `ETag` now names the encoded
  representation, but the gateway is not a cache and clients do not validate
  through it; removing it would discard useful provider metadata. This is a
  conscious trade-off, recorded here rather than silently ignored.
- Trailer frames from the upstream are dropped when decoding, because the
  downstream response is re-framed. Non-decoded responses keep today's
  behavior.
- `Content-Encoding: identity` is treated as unencoded and passed through
  unchanged.

### 5. Dependencies

```toml
async-compression = { version = "0.4", default-features = false,
                      features = ["tokio", "gzip", "zlib", "brotli", "zstd"] }
```

gzip/zlib/brotli use pure-Rust backends. `zstd` goes through `zstd-sys`, which
needs a C toolchain. No `tokio-util` or `futures-util` production dependency
is needed because `ReadBody` and `BodyBufReader` are small local adapters.

### 6. No configuration switch

The behavior is always on, as decided. There is no new configuration field,
so configuration validation and the `configuration-validation` spec are
unchanged.

## Risks / Trade-offs

- **zstd build environment**: `zstd-sys` requires a C compiler on Windows
  (MSVC) and inside the `cargo cross` images. Mitigation: verify Windows and
  cross builds early in implementation; if a target cannot provide a C
  toolchain, the fallback is to drop `zstd` from the supported set and from
  the advertised intersection (a scope reduction, not a silent behavior
  change). Windows/MSVC verification passed during implementation (the
  bundled C sources compiled). Cross-target verification is deferred to CI,
  because no local environment could run `cross`.
- **`deflate` interoperability**: some servers emit raw DEFLATE under
  `Content-Encoding: deflate`. The gateway follows the RFC and expects zlib
  framing; such servers will produce a mid-stream decode failure. It will not
  guess codecs, because guessing would make corrupt streams look valid.
- **No expansion limit**: a malicious or broken upstream could expand one
  chunk dramatically. Accepted by decision; upstreams are
  operator-configured. The decoder still allocates per poll rather than
  buffering the whole response.
- **Loss of encoding metadata**: removing `Content-Encoding`/`Content-MD5`
  and leaving a now-misleading strong `ETag` changes representation
  metadata. Documented; the alternative (re-compressing for the client) is
  explicitly out of scope.
- **Plaintext on encrypted hops**: when a gateway peer responds, the
  intermediate gateway decodes before forwarding, so the encrypted link
  carries plaintext and its bandwidth rises. This matches the plaintext
  invariant but is worth documenting in the e2ee notes.
- **Trailers**: decoding drops HTTP trailers. LLM providers do not use them;
  no current tests depend on trailer passthrough for coded responses.
- **206 Partial Content**: a range response with a content coding cannot be
  decoded independently. It will fail mid-stream. Range requests are not part
  of the LLM use case and are documented as unsupported for coded responses.

## Implementation Sketch

```rust
// forward.rs, before hop-by-hop removal
headers.insert(ACCEPT_ENCODING, rewritten_value); // or remove

// server::handle, after into_parts()
match decode::classify(&parts.headers) {
    Ok(None) => { /* existing path */ }
    Ok(Some(coding)) => {
        parts.headers.remove(CONTENT_ENCODING);
        parts.headers.remove(CONTENT_LENGTH);
        parts.headers.remove(CONTENT_MD5);
        let body = decode::ReadBody::new(body, coding);
        // expected_response_bytes = None
    }
    Err(Unsupported) => {
        request_state.set_status(502);
        request_state.fail("upstream_encoding");
        return Ok(finish_local(request_state, 502, "Bad Gateway\n"));
    }
}
```

## Testing Approach

- Unit tests for header filtering: multiple field lines, wildcard, q-values,
  q=0, unsupported-only, and no header.
- Unit tests for `classify`: absent, `identity`, each supported coding,
  mixed case, comma-separated multi-layer, unsupported token.
- Round-trip tests for all four decoders with the encoded stream split at
  awkward boundaries (header split, mid-block, trailer split), plus truncated
  input and empty input.
- End-to-end tests with an upstream that compresses only when asked:
  plaintext delivery, `Accept-Encoding` reduction, 502 for unexpected
  coding, no-body statuses untouched, gzip SSE delivered incrementally,
  conversation record containing plaintext, and incomplete state on
  mid-stream corruption.
- Build checks on Windows and a `cargo cross` target for the `zstd-sys`
  toolchain requirement.
