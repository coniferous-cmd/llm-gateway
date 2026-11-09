---
create-time: 2026-11-09 09:47:43
---

## MODIFIED Requirements

### Requirement: Transparent forwarding

The system SHALL forward requests to the address formed by joining the selected target's `base_url` with the original path, or with the path produced by the selected entry's first matching rewrite rule. The raw query string and query delimiter presence SHALL remain unchanged. The system SHALL set HTTP `Host` to the selected target's authority (hostname plus explicit port). Beyond the configured path rewriting, the `Authorization` header is replaced with the target's `api_key`, the `Accept-Encoding` header is replaced per the plaintext response delivery requirement, and, when model-based routing matched a declaration with `id != name`, the top-level `model` field of a JSON object body is rewritten per the model name rewrite requirement. All other application-layer request headers and body content SHALL be forwarded as-is, with no forwarding headers such as `X-Forwarded-For` added or appended. Hop-by-hop headers may be removed per standard HTTP proxy rules (including headers named by `Connection`). The provider's response status code and application-layer headers SHALL be passed back as-is, except that content-coding headers are adjusted when the body is decoded and that unsupported or multi-layer codings are rejected, per the plaintext response delivery requirement; streaming responses SHALL be forwarded chunk by chunk, not buffered until the response ends.

#### Scenario: URL joining

- **WHEN** the authenticated request is `POST /v1/chat/completions?stream=true`, no path rewrite rule matches, and the selected target's `base_url` is `https://api.openai.com`
- **THEN** the forwarding target is `https://api.openai.com/v1/chat/completions?stream=true` and the HTTP `Host` is `api.openai.com`, not the address the caller used to reach the gateway

#### Scenario: Only configured transformations are applied

- **WHEN** a request carrying `Bearer sk-virt-dev` is forwarded without a matching path rewrite rule or a model-name alias transformation
- **THEN** the upstream receives `Authorization` set to the selected target's real `api_key`, `Host` set to the selected target's authority, `Accept-Encoding` reduced to the gateway-supported subset of the caller's acceptable codings, and the request body bytes and remaining application-layer headers unchanged; no forwarding headers are added, and only standard HTTP hop-by-hop header handling is allowed

#### Scenario: Streaming response passthrough

- **WHEN** the provider returns an SSE streaming response (`text/event-stream`) after receiving a rewritten path, with or without a supported content coding
- **THEN** the response body is passed back chunk by chunk without being buffered until the response ends or until decoding completes

#### Scenario: URL joining without a matching rule

- **WHEN** the request is `POST /v1/chat/completions?stream=true`, no rule matches, and the selected target's `base_url` is `https://api.openai.com`
- **THEN** the forwarding target is `https://api.openai.com/v1/chat/completions?stream=true` and HTTP `Host` is `api.openai.com`

#### Scenario: URL joining after rewriting

- **WHEN** the request is `POST /api/chat/completions?next=/api&x=a%2Fb&x=2`, the selected rule is `/api:/v1`, and `base_url` is `https://api.openai.com`
- **THEN** the forwarding target is `https://api.openai.com/v1/chat/completions?next=/api&x=a%2Fb&x=2`, preserving query bytes and order

### Requirement: Model name rewrite

When model-based routing matches a model declaration, the system SHALL forward `name` (not `id`) to the provider: if `id != name`, the gateway SHALL replace the value of the top-level `model` field in the JSON object body with `name` before forwarding; if `id == name`, the body bytes are forwarded unchanged. The rewrite SHALL NOT alter any other body field, and the response from the provider is delivered subject to the plaintext response delivery requirement.

#### Scenario: Alias rewrite

- **WHEN** the config maps `id: mimo-flash` to `name: mimo-v2.6-flash` and an authenticated request body is `{"model":"mimo-flash","messages":[...]}`
- **THEN** the provider receives `{"model":"mimo-v2.6-flash","messages":[...]}` with all other fields identical

#### Scenario: id equals name

- **WHEN** the matched declaration has `id: mimo-v2.6-flash` and `name: mimo-v2.6-flash`
- **THEN** the body reaches the provider byte-for-byte unchanged

## ADDED Requirements

### Requirement: Plaintext response delivery

The system SHALL ensure that every response body it forwards to the caller is unencoded, and SHALL NOT send `Content-Encoding` or a `Content-Length` derived from an encoded representation. Before forwarding, the system SHALL replace the caller's `Accept-Encoding` with the intersection of the caller's acceptable codings and the gateway-supported set `{gzip, deflate, br, zstd}`: only codings with a positive quality value are acceptable, `*` stands for the whole supported set, an empty intersection removes the header, and a caller that sent no `Accept-Encoding` causes the gateway to add none. When an upstream response carries exactly one supported coding, the system SHALL decode the body incrementally, treat `deflate` as zlib-wrapped DEFLATE, and remove `Content-Encoding`, `Content-Length`, and `Content-MD5` before forwarding; decoding SHALL begin as bytes arrive and SHALL NOT wait for the response to end. When the upstream response carries no content coding or `identity`, the body SHALL pass through unchanged. Responses without a body (`HEAD`, `204`, `205`, `304`) SHALL pass through unchanged. A response carrying an unsupported coding, or more than one coding, SHALL be rejected with `502 Bad Gateway` before any body byte is written, and without forwarding the encoded body. An empty body carrying a content coding SHALL be forwarded as an empty unencoded body with coding-derived headers removed. If decoding fails after the response has started, the system SHALL abort the body stream so the caller cannot mistake truncated data for a complete reply.

#### Scenario: Caller requests gzip and receives plaintext

- **WHEN** the caller sends `Accept-Encoding: gzip, br` and the provider returns `200` with `Content-Encoding: gzip` and a gzip body
- **THEN** the provider receives `Accept-Encoding: gzip, br` and the caller receives the decoded body with no `Content-Encoding` and no `Content-Length`

#### Scenario: Caller advertises only brotli

- **WHEN** the caller sends `Accept-Encoding: br` and the provider returns a brotli-encoded body
- **THEN** the gateway decodes brotli and the caller receives the plaintext body

#### Scenario: Caller advertises an unsupported coding only

- **WHEN** the caller sends `Accept-Encoding: compress` and the provider would compress only when asked
- **THEN** the gateway removes `Accept-Encoding` upstream, the provider returns an unencoded body, and the caller receives that body unchanged

#### Scenario: Caller sends no Accept-Encoding

- **WHEN** the caller sends no `Accept-Encoding`
- **THEN** the gateway adds none upstream and the caller receives the provider's unencoded body unchanged

#### Scenario: Wildcard and quality values

- **WHEN** the caller sends `Accept-Encoding: *` or `Accept-Encoding: gzip;q=0, deflate`
- **THEN** the gateway advertises all four supported codings in the first case, and only `deflate` in the second

#### Scenario: Zero-length body with a coding

- **WHEN** the provider returns a response with `Content-Encoding: gzip` and no body bytes
- **THEN** the caller receives an empty body with `Content-Encoding` and `Content-Length` removed

#### Scenario: Unsupported or multi-layer coding

- **WHEN** the provider returns an unsupported coding such as `Content-Encoding: compress`, or returns more than one coding such as `Content-Encoding: gzip, br`
- **THEN** the caller receives `502 Bad Gateway` and no byte of the encoded body is forwarded

#### Scenario: Streaming decode

- **WHEN** the provider returns a gzip-encoded SSE response and flushes one event before producing the rest
- **THEN** the caller receives the decoded first event before the provider has produced the remaining events

#### Scenario: Decode failure mid-stream

- **WHEN** the encoded response body is truncated or corrupt after response headers have been forwarded
- **THEN** the response stream ends with an error rather than a complete body, and the call is recorded as incomplete

#### Scenario: No-body responses stay untouched

- **WHEN** the response is to a `HEAD` request, or carries status `204`, `205`, or `304`, together with a content coding header
- **THEN** the response headers and the absence of a body are forwarded unchanged
