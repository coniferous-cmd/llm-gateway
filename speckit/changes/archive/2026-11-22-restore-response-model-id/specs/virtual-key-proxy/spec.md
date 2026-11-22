---
create-time: 2026-11-22 10:11:58
---

## ADDED Requirements

### Requirement: Response model ID restoration

For eligible successful OpenAI Chat Completions and Anthropic Messages responses with a matched model declaration, the gateway SHALL replace the existing string at the eligible model field with the exact ID resolved for that HTTP request. The field SHALL be `model` in ordinary JSON and Chat Completions SSE chunks, and `message.model` in the Anthropic SSE `message_start` event. The gateway SHALL preserve all other payload fields, subject to the respective inspection requirements.

#### Scenario: Successful JSON response

- **WHEN** a caller selects `alias-a`, mapped to `claude-real`, and the provider sends a successful JSON message with `"model":"claude-real-20261101"`
- **THEN** the caller receives `"model":"alias-a"` with all other payload fields preserved

#### Scenario: Concurrent requests share an upstream model

- **WHEN** two concurrent requests use `alias-a` and `alias-b`, both mapped to the same provider model, and their responses arrive in reverse order
- **THEN** each HTTP response contains the ID selected for its own request

#### Scenario: Streaming response model fields

- **WHEN** a routed OpenAI Chat Completions stream sends an event containing `model` or an Anthropic Messages stream sends a `message_start` containing `message.model`, and the complete event fits within the inspection limit, even if split across upstream body chunks
- **THEN** the caller receives each completed event with the eligible model field set to the resolved ID before the provider finishes the stream, and other events and stream markers retain their original contents

#### Scenario: Anthropic message start preserves event bytes

- **WHEN** an Anthropic `message_start` event within the inspection limit contains valid JSON with a string at `message.model`, including when its JSON is spread across several `data:` lines
- **THEN** the gateway replaces only that string literal using Go `encoding/json` escaping and preserves every other byte of the event, including unrelated `model` fields, line endings, and the event delimiter

#### Scenario: Anthropic message start lacks a usable nested model

- **WHEN** an Anthropic `message_start` event contains malformed JSON, lacks a top-level `message` object, or lacks a string `model` directly inside that object
- **THEN** the gateway forwards the event byte for byte

#### Scenario: Resolved ID includes a suffix

- **WHEN** model routing resolves `alias-a[1m]` via a declaration for `alias-a`
- **THEN** an eligible response model field contains `alias-a[1m]`

#### Scenario: Provider response is outside the rewrite scope

- **WHEN** the provider returns an HTTP error, a response to a request with no matched declaration, a response from an unrelated endpoint, or a payload with an absent or non-string model field
- **THEN** the gateway relays the payload without model rewriting

### Requirement: SSE inspection and partial event delivery

For SSE eligible for model ID restoration, the gateway SHALL buffer at most 1 MiB of decoded input per event, including its blank-line delimiter. It SHALL forward an oversized event unchanged as bytes arrive and resume inspection at the next event. It SHALL emit buffered bytes of an unterminated event unchanged before reporting clean EOF or propagating the original upstream read or decode error.

#### Scenario: Oversized event without a delimiter

- **WHEN** an eligible SSE event exceeds 1 MiB before its terminating blank line arrives
- **THEN** the gateway forwards its buffered prefix and subsequent bytes unchanged and in order as they become available, without waiting for a delimiter or end of stream

#### Scenario: Inspection resumes after an oversized event

- **WHEN** the delimiter of an oversized event and a following eligible event arrive in the same body frame
- **THEN** the oversized event passes through unchanged and the following event is eligible for rewriting under its own 1 MiB limit

#### Scenario: Stream ends with a partial event

- **WHEN** the upstream SSE body ends normally or raises a read or decode error while an unterminated event is buffered
- **THEN** the gateway forwards those bytes exactly once without adding a delimiter or rewriting the partial payload, then reports EOF or propagates the original body error and records the response as incomplete in the error case

### Requirement: Non-streaming JSON inspection

For a non-streaming JSON response otherwise eligible for model ID restoration, the gateway SHALL inspect the JSON only after a clean end of body and SHALL limit inspection to 32 MiB of decoded data. It SHALL forward over-limit bodies and complete bodies containing invalid JSON without rewriting. If the upstream read or decode fails, the gateway SHALL propagate the body error and record the response as incomplete.

#### Scenario: JSON model spans multiple frames

- **WHEN** a valid JSON body of up to and including 32 MiB ends cleanly and its top-level `model` key or string value is split across data frames, including after content decoding
- **THEN** the caller receives the body with the model ID restored and all other payload bytes preserved

#### Scenario: Decoded JSON exceeds the inspection limit

- **WHEN** an otherwise eligible decoded JSON body exceeds 32 MiB
- **THEN** the caller receives all decoded payload bytes in order, including the original `model`, without waiting for EOF after the gateway detects that the limit was exceeded

#### Scenario: Response body ends with invalid JSON

- **WHEN** the response body ends cleanly but contains invalid JSON, including a body that ends after a complete `model` string and before the closing object delimiter
- **THEN** the caller receives the original decoded payload bytes

#### Scenario: Response fails during inspection

- **WHEN** an upstream body read or decoding error occurs before inspection finishes
- **THEN** the caller sees a body failure and the gateway records the response as incomplete

## MODIFIED Requirements

### Requirement: Model name rewrite

When model-based routing matches a model declaration and `id != name`, the gateway SHALL replace only the top-level `model` value in the JSON request body with `name` before forwarding. When `id == name`, the gateway SHALL forward the request body byte for byte. In both cases, the gateway SHALL apply the Plaintext response delivery requirement to the provider response, then apply the Response model ID restoration requirement to the resulting plaintext body where eligible.

#### Scenario: Alias rewrite

- **WHEN** the config maps `id: mimo-flash` to `name: mimo-v2.6-flash` and an authenticated request body is `{"model":"mimo-flash","messages":[...]}`
- **THEN** the provider receives `{"model":"mimo-v2.6-flash","messages":[...]}` with all other fields identical

#### Scenario: id equals name

- **WHEN** the matched declaration has `id: mimo-v2.6-flash` and `name: mimo-v2.6-flash`
- **THEN** the body reaches the provider byte-for-byte unchanged

### Requirement: Transparent forwarding

The system SHALL forward requests to the address formed by joining the selected target's `base_url` with the original path, or with the path produced by the selected entry's first matching rewrite rule. The raw query string and query delimiter presence SHALL remain unchanged. The system SHALL set HTTP `Host` to the selected target's authority (hostname plus explicit port). Beyond the configured path rewriting, the `Authorization` header is replaced with the target's `api_key`, the `Accept-Encoding` header is replaced per the plaintext response delivery requirement, and, when model-based routing matched a declaration with `id != name`, the top-level `model` field of a JSON object body is rewritten per the model name rewrite requirement. All other application-layer request headers and body content SHALL be forwarded as-is, with no forwarding headers such as `X-Forwarded-For` added or appended. Hop-by-hop headers may be removed per standard HTTP proxy rules (including headers named by `Connection`). The provider's response status code and application-layer headers SHALL be passed back as-is, except that content-coding headers are adjusted when the body is decoded and that unsupported or multi-layer codings are rejected, per the plaintext response delivery requirement. Headers describing the original body length or integrity SHALL be removed or recomputed when response model ID restoration can change the forwarded body. Streaming responses SHALL be forwarded chunk by chunk, not buffered until the response ends.

#### Scenario: URL joining

- **WHEN** the authenticated request is `POST /v1/chat/completions?stream=true`, no path rewrite rule matches, and the selected target's `base_url` is `https://api.openai.com`
- **THEN** the forwarding target is `https://api.openai.com/v1/chat/completions?stream=true` and the HTTP `Host` is `api.openai.com`, not the address the caller used to reach the gateway

#### Scenario: Streaming response passthrough

- **WHEN** the provider returns an SSE streaming response (`text/event-stream`) outside model ID restoration after receiving a rewritten path, with or without a supported content coding
- **THEN** the response body is passed back chunk by chunk without being buffered until the response ends or until decoding completes

#### Scenario: URL joining without a matching rule

- **WHEN** the request is `POST /v1/chat/completions?stream=true`, no rule matches, and the selected target's `base_url` is `https://api.openai.com`
- **THEN** the forwarding target is `https://api.openai.com/v1/chat/completions?stream=true` and HTTP `Host` is `api.openai.com`

#### Scenario: URL joining after rewriting

- **WHEN** the request is `POST /api/chat/completions?next=/api&x=a%2Fb&x=2`, the selected rule is `/api:/v1`, and `base_url` is `https://api.openai.com`
- **THEN** the forwarding target is `https://api.openai.com/v1/chat/completions?next=/api&x=a%2Fb&x=2`, preserving query bytes and order

#### Scenario: Only configured transformations are applied

- **WHEN** a request carrying `Bearer sk-virt-dev` is forwarded without a matching path rewrite rule or a model-name alias transformation
- **THEN** the upstream receives `Authorization` set to the selected target's real `api_key`, `Host` set to the selected target's authority, `Accept-Encoding` reduced to the gateway-supported subset of the caller's acceptable codings, and the request body bytes and remaining application-layer headers unchanged; no forwarding headers are added, and only standard HTTP hop-by-hop header handling is allowed

### Requirement: Plaintext response delivery

The system SHALL ensure that every response body it forwards to the caller is unencoded, and SHALL NOT send `Content-Encoding` or a `Content-Length` derived from an encoded representation. Before forwarding, the system SHALL replace the caller's `Accept-Encoding` with the intersection of the caller's acceptable codings and the gateway-supported set `{gzip, deflate, br, zstd}`: only codings with a positive quality value are acceptable, `*` stands for the whole supported set, an empty intersection removes the header, and a caller that sent no `Accept-Encoding` causes the gateway to add none. When an upstream response carries exactly one supported coding, the system SHALL decode the body incrementally, treat `deflate` as zlib-wrapped DEFLATE, and remove `Content-Encoding`, `Content-Length`, and `Content-MD5` before forwarding; decoding SHALL begin as bytes arrive and SHALL NOT wait for the response to end. When the upstream response carries no content coding or `identity`, the body SHALL pass through unchanged except for the response model ID restoration requirement. Responses without a body (`HEAD`, `204`, `205`, `304`) SHALL pass through unchanged. A response carrying an unsupported coding, or more than one coding, SHALL be rejected with `502 Bad Gateway` before any body byte is written, and without forwarding the encoded body. An empty body carrying a content coding SHALL be forwarded as an empty unencoded body with coding-derived headers removed. If decoding fails after the response has started, the system SHALL abort the body stream so the caller cannot mistake truncated data for a complete reply.

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
