# virtual-key-proxy Specification

## Purpose

TBD — purpose statement for the virtual-key-proxy capability.

## Requirements

### Requirement: Virtual key authentication

The system SHALL accept HTTP requests carrying an `Authorization: Bearer <token>` header and look the token up in the configuration loaded at startup. On a hit the request proceeds; on a miss the system returns `401 Unauthorized`. The system SHALL NOT perform any validation beyond virtual key authentication (no restrictions on path, HTTP method, Content-Type, or request body content).

#### Scenario: Valid virtual key

- **WHEN** a request carries `Authorization: Bearer sk-virt-dev` and that token exists in the configuration
- **THEN** the request is accepted and enters the forwarding flow, and is not rejected because of its path or method

#### Scenario: Invalid or missing virtual key

- **WHEN** a request lacks the `Authorization` header, its format is not `Bearer <token>`, or the token is not in the configuration
- **THEN** the system returns `401 Unauthorized` and forwards the request to no provider

### Requirement: Configuration file loading

The system SHALL load the virtual key and target mappings from the YAML configuration file once at startup and SHALL NOT reload the configuration while running (no hot reload). Each entry declares one `token`, one `target`, and MAY declare `models` (a list of `{id, name}`). Targets use a bare-origin `base_url` or an encrypted `peer` reference with an optional `api_key`, subject to the existing target validation rules, and MAY declare `rewrite` (an ordered list of path prefix rules) within `target`. The same `token` may appear multiple times, and the system SHALL resolve it into an ordered entry list for that token, each entry carrying its own target, model declarations, and rewrite rules.

#### Scenario: One token with multiple targets

- **WHEN** `sk-virt-dev` appears three times in the config file with `base_url` values for OpenAI, DeepSeek, and local Ollama respectively
- **THEN** after startup, requests authenticated with `sk-virt-dev` can be routed to any of those three addresses, retaining the selected entry's own rewrite rules

#### Scenario: base_url normalization

- **WHEN** `base_url` is `http://127.0.0.1:11434/` or `http://127.0.0.1:11434`
- **THEN** both normalize to the same origin, which is joined with the original or explicitly rewritten path without introducing an additional slash

### Requirement: Per-entry model declarations

Each configuration key entry MAY declare a `models` list alongside `target`, where every item has a required `id` and a required `name` (non-empty strings after trimming). `id` is the model name callers put in their requests; `name` is the model name the gateway passes to the provider. Within one token, `id` values MUST be unique across all entries; a duplicate `id` is a startup validation failure. Entries of the same token may declare different `models` lists. A config without `models` remains valid and behaves exactly as before.

#### Scenario: Config with models

- **WHEN** the config declares a key entry with `models: [{id: mimo-v2.6-flash, name: mimo-v2.6-flash}, {id: mimo-v2.6-pro, name: mimo-v2.6-pro}]`
- **THEN** startup succeeds and the entry can serve both model ids

#### Scenario: Duplicate id within one token

- **WHEN** two entries under the same `token` both declare `id: mimo-v2.6-flash`
- **THEN** startup fails with an error naming the offending `id`, and the gateway does not start

#### Scenario: Missing or empty id / name

- **WHEN** a `models` item has an empty `id` or empty `name`
- **THEN** startup fails with an error identifying the offending field and line

### Requirement: Model-based routing

When the authenticated token declares at least one model `id` across its entries, an authenticated request whose JSON object body carries a top-level `model` string field SHALL be routed by (virtual key, model id): the request is forwarded to the target of the key entry whose `models` list contains an `id` equal to the body's `model` value, and the token's round-robin counter is not advanced. If the `model` value matches no `id` under the token, the system SHALL return `400 Bad Request` and contact no provider. Requests that carry no resolvable model id — non-JSON body, JSON body without a top-level `model` string field, or no body — SHALL fall back to the existing round-robin selection across all entries of the token, and their bodies SHALL NOT be modified. A token that declares no `models` on any entry keeps today's behavior exactly: no body inspection at all, round-robin across its entries.

#### Scenario: Model selects the provider entry

- **WHEN** token `sk-dev` has entry 1 (`target` A, models `mimo-v2.6-flash`) and entry 2 (`target` B, models `mimo-v2.6-pro`), and an authenticated request body is `{"model": "mimo-v2.6-pro", ...}`
- **THEN** the request is forwarded to target B, and target A receives nothing

#### Scenario: Unknown model id under the key

- **WHEN** the authenticated token declares model ids and the body's `model` value matches none of them
- **THEN** the gateway returns `400 Bad Request` and contacts no provider

#### Scenario: Request without a model field

- **WHEN** an authenticated request with a non-JSON body arrives
- **THEN** the gateway performs no model lookup, forwards the body bytes unchanged, and selects the target by round-robin across the token's entries

#### Scenario: Token declares no models

- **WHEN** a config predates `models` (no entry of the token declares `models`) and a request body carries any `model` value
- **THEN** the request is forwarded unchanged by round-robin across the token's entries, exactly as before this change

### Requirement: Local model listing

A bodyless `GET` whose path, after the selected target's rewrite rules, equals `/v1/models` SHALL be answered by the gateway itself, contacting no provider. The response SHALL list the `id` of every model declared by a live entry of the authenticated token, once each, in declaration order; models on entries past their `expire` time SHALL be omitted. The token's round-robin counter SHALL NOT advance. When the request carries an `anthropic-version` header the Anthropic shape SHALL be returned (`data` of `{type: "model", id, ...}`, `has_more: false`, `first_id`, `last_id`); otherwise the OpenAI shape (`object: "list"`, `data` of `{id, object: "model", ...}`). A token that declares no `models` SHALL receive an empty `data` list, never the provider's catalog.

#### Scenario: Listing for a model-enabled key

- **WHEN** token `sk-dev` declares `mimo-v2.6-flash` on one entry and `mimo-v2.6-pro` on another, and the request is a bodyless `GET /v1/models`
- **THEN** the gateway returns both ids and no provider is contacted

#### Scenario: Listing uses the Anthropic shape on request

- **WHEN** the same request carries `anthropic-version`
- **THEN** the response uses `data[].type = "model"` and `has_more = false`

#### Scenario: Listing for a legacy key

- **WHEN** a token declares no `models` and the request is a bodyless `GET /v1/models`
- **THEN** the gateway returns an empty `data` list and no provider is contacted

#### Scenario: Listing path after rewriting

- **WHEN** the selected target maps `/anthropic` to the empty prefix and the request is `GET /anthropic/v1/models`
- **THEN** the gateway answers locally because the rewritten path is `/v1/models`

### Requirement: Model name rewrite

When model-based routing matches a model declaration and `id != name`, the gateway SHALL replace only the top-level `model` value in the JSON request body with `name` before forwarding. When `id == name`, the gateway SHALL forward the request body byte for byte. In both cases, the gateway SHALL apply the Plaintext response delivery requirement to the provider response, then apply the Response model ID restoration requirement to the resulting plaintext body where eligible.

#### Scenario: Alias rewrite

- **WHEN** the config maps `id: mimo-flash` to `name: mimo-v2.6-flash` and an authenticated request body is `{"model":"mimo-flash","messages":[...]}`
- **THEN** the provider receives `{"model":"mimo-v2.6-flash","messages":[...]}` with all other fields identical

#### Scenario: id equals name

- **WHEN** the matched declaration has `id: mimo-v2.6-flash` and `name: mimo-v2.6-flash`
- **THEN** the body reaches the provider byte-for-byte unchanged

### Requirement: Round-robin routing

When one virtual key maps to multiple targets, requests that carry no resolvable model id SHALL select a target in round-robin order (atomic counter modulo) across all entries of the token, distributing requests evenly across targets; model-matched requests SHALL select the matching entry directly and SHALL NOT advance the round-robin counter. Routing SHALL remain race-free under concurrent requests.

#### Scenario: Round-robin across multiple targets

- **WHEN** `sk-virt-dev` is configured with 3 targets and 6 authenticated requests without a model field arrive in sequence
- **THEN** the 6 requests hit targets 1, 2, 3, 1, 2, 3 in order (each target selected exactly twice)

#### Scenario: Single target

- **WHEN** the virtual key is configured with only 1 target
- **THEN** all authenticated requests are forwarded to that target

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

### Requirement: Errors relayed as-is

When the target provider is unreachable or returns an HTTP error status, the system SHALL relay the error status (and response body) to the caller as-is; the system SHALL NOT retry and SHALL NOT switch to another target of the same key (no failover).

#### Scenario: Provider unreachable

- **WHEN** the selected target address refuses the connection or times out
- **THEN** the caller receives an error response indicating upstream connection failure (e.g. `502 Bad Gateway`), and the gateway does not retry other targets

#### Scenario: Provider returns an error status

- **WHEN** the provider returns `429 Too Many Requests` with its original response body
- **THEN** the caller receives `429` and the same response body

### Requirement: Per-entry path rewrite configuration

Each key entry's `target` MAY declare `rewrite` alongside `base_url` (or `peer`) and `api_key`; the configuration field is `keys[].target.rewrite`, not an entry-level field. The system SHALL accept an ordered list of strings in `source:destination` form, validate and parse it at startup, and retain it separately for each entry even when entries share a token. Omitted `rewrite` or an empty list SHALL mean no path rewriting. Each rule SHALL contain exactly one literal colon separator. The source SHALL be a non-empty absolute path prefix beginning with `/`; the destination SHALL be either another such prefix or the single marker `-`, meaning delete the matched prefix. An empty destination SHALL remain invalid. Path prefixes SHALL NOT contain a URL scheme, authority (a leading `//`), query, fragment, whitespace, or malformed percent escapes. Prefixes are literal escaped URL path strings, not regular expressions. A scalar instead of a list, a non-string item, or an invalid rule SHALL fail startup with an error identifying the key entry, offending field or item, and YAML line. The existing bare-origin restriction for `base_url` SHALL remain unchanged.

#### Scenario: Multiple rewrite rules

- **WHEN** a key entry's `target` declares `rewrite: ['/api:/v1', '/openai:/v1']`
- **THEN** startup succeeds and preserves the two rules in declaration order for that entry

#### Scenario: Backward compatible configuration

- **WHEN** an entry's `target` omits `rewrite` or declares `rewrite: []`
- **THEN** startup succeeds and requests selected for that entry retain their original paths

#### Scenario: Invalid rewrite configuration

- **WHEN** `target.rewrite` is a scalar, contains a non-string item, or includes a rule such as `/api`, `:/v1`, `/api:`, `api:/v1`, `/api:https://example.com`, or `/api:/v1?x=1`
- **THEN** startup fails with a configuration error locating the offending entry and YAML line

### Requirement: Ordered path prefix rewriting

After model routing or round-robin selects a key entry, the system SHALL apply only that entry's rewrite rules to the outgoing request path. It SHALL compare rules in order against the original escaped request path, using case-sensitive literal matching. A source matches when the path equals the source, or when the path starts with the source and the match ends at a path-segment boundary: the source ends in `/` or the next path character is `/`. The system SHALL replace the matched prefix exactly once with the destination and append the untouched suffix, then stop. When the destination is `-`, it SHALL delete the matched prefix, preserving the escaped suffix and prepending `/` only if needed to keep an absolute path; an empty result SHALL become `/`. It SHALL NOT apply subsequent rules to the rewritten result, otherwise normalize slashes, clean dot segments, or decode encoded separators for matching. No match SHALL leave the path unchanged. These rules SHALL apply equally to plaintext upstreams and encrypted peer targets, and SHALL NOT change routing selection, counters, model-name rewriting, request methods, or responses.

#### Scenario: Delete prefix marker configuration

- **WHEN** a target declares `rewrite: ['/api:-', '/keep:/-']`
- **THEN** startup accepts `-` as the deletion marker in the first rule and `/-` as a literal destination in the second rule; `/api:` remains invalid

#### Scenario: Delete matching prefix once

- **WHEN** the selected target declares `/api:-` and requests use `/api/v1/messages`, `/api/api/models`, `/api`, and `/api/`
- **THEN** the outgoing paths are `/v1/messages`, `/api/models`, `/`, and `/` respectively; `/apix/models` and `/api%2Fmodels` remain unchanged

#### Scenario: Delete prefix preserves encoding and query

- **WHEN** the rules are `['/api:-', '/v1:/v2']` and the request is `/api/v1/a%2Fb%20c?next=/api&x=1&x=2`
- **THEN** the outgoing URI is `/v1/a%2Fb%20c?next=/api&x=1&x=2`, without applying the second rule

#### Scenario: Delete a prefix ending in a slash

- **WHEN** the selected rule is `/api/:-` and requests use `/api/models` and `/api/`
- **THEN** the outgoing paths are `/models` and `/` respectively, while `/api` does not match this rule

#### Scenario: Exact and descendant paths

- **WHEN** the selected entry declares `/api:/v1` and requests use `/api`, `/api/`, and `/api/chat/completions`
- **THEN** the upstream paths are `/v1`, `/v1/`, and `/v1/chat/completions` respectively

#### Scenario: Complete segment boundary

- **WHEN** the selected entry declares `/api:/v1` and a request uses `/apix/models` or `/other/api/models`
- **THEN** the path remains unchanged

#### Scenario: First match wins rather than longest match

- **WHEN** rules are `['/api:/v1', '/api/admin:/admin']` and the path is `/api/admin/models`
- **THEN** the upstream path is `/v1/admin/models`, because the first matching rule wins

#### Scenario: Multiple client prefixes

- **WHEN** rules are `['/api:/v1', '/openai:/v1']` and the path is `/openai/models`
- **THEN** the upstream path is `/v1/models`

#### Scenario: No chained rewriting

- **WHEN** rules are `['/api:/v1', '/v1:/v2']` and the path is `/api/api/models`
- **THEN** the upstream path is `/v1/api/models`, not `/v2/api/models`

#### Scenario: Selected entry owns its rules

- **WHEN** two entries of one token declare different rules and model routing selects one of them
- **THEN** only the model-selected entry's rules apply, and the round-robin counter does not advance

#### Scenario: Round-robin retains entry-specific rules

- **WHEN** two entries of one token map `/api` to `/v1` and `/v2` respectively, and two sequential requests without a model select those entries in order
- **THEN** the upstream paths use `/v1` and `/v2` respectively

#### Scenario: Encoded suffix remains intact

- **WHEN** the rule is `/api:/v1` and the request path is `/api/files/a%2Fb%20c`
- **THEN** the outgoing escaped path is `/v1/files/a%2Fb%20c`, without decoding or double-encoding the suffix

#### Scenario: Encoded separator is not a boundary

- **WHEN** the rule is `/api:/v1` and the escaped request path is `/api%2Fmodels`
- **THEN** the path remains unchanged because `%2F` is not a literal segment separator
