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

- **WHEN** an authenticated `GET /v1/models` or a request with a non-JSON body arrives
- **THEN** the gateway performs no model lookup, forwards the body bytes unchanged, and selects the target by round-robin across the token's entries

#### Scenario: Token declares no models

- **WHEN** a config predates `models` (no entry of the token declares `models`) and a request body carries any `model` value
- **THEN** the request is forwarded unchanged by round-robin across the token's entries, exactly as before this change

### Requirement: Model name rewrite

When model-based routing matches a model declaration, the system SHALL forward `name` (not `id`) to the provider: if `id != name`, the gateway SHALL replace the value of the top-level `model` field in the JSON object body with `name` before forwarding; if `id == name`, the body bytes are forwarded unchanged. The rewrite SHALL NOT alter any other body field, and the response from the provider is passed back as-is.

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

The system SHALL forward requests to the address formed by joining the selected target's `base_url` with the original path, or with the path produced by the selected entry's first matching rewrite rule. The raw query string and query delimiter presence SHALL remain unchanged. The system SHALL set HTTP `Host` to the selected target's authority (hostname plus explicit port). Beyond the configured path rewriting, only the `Authorization` header is replaced with the target's `api_key` and, when model-based routing matched a declaration with `id != name`, the top-level `model` field of a JSON object body is rewritten per the model name rewrite requirement. All other application-layer request headers and body content SHALL be forwarded as-is, with no forwarding headers such as `X-Forwarded-For` added or appended. Hop-by-hop headers may be removed per standard HTTP proxy rules (including headers named by `Connection`). The provider's response (status code, response headers, and SSE response body) SHALL be passed back as-is; streaming responses SHALL be forwarded chunk by chunk, not buffered until the response ends.

#### Scenario: URL joining

- **WHEN** the authenticated request is `POST /v1/chat/completions?stream=true`, no path rewrite rule matches, and the selected target's `base_url` is `https://api.openai.com`
- **THEN** the forwarding target is `https://api.openai.com/v1/chat/completions?stream=true` and the HTTP `Host` is `api.openai.com`, not the address the caller used to reach the gateway

#### Scenario: Only the Authorization header is replaced

- **WHEN** a request carrying `Bearer sk-virt-dev` is forwarded without a matching path rewrite rule or a model-name alias transformation
- **THEN** the upstream receives `Authorization` set to the selected target's real `api_key`, the HTTP `Host` set to the selected target's authority, and the request body bytes and remaining application-layer headers unchanged; no forwarding headers are added, and only standard HTTP hop-by-hop header handling is allowed

#### Scenario: Streaming response passthrough

- **WHEN** the provider returns an SSE streaming response (`text/event-stream`) after receiving a rewritten path
- **THEN** the response body is passed back chunk by chunk without being buffered or rewritten

#### Scenario: URL joining without a matching rule

- **WHEN** the request is `POST /v1/chat/completions?stream=true`, no rule matches, and the selected target's `base_url` is `https://api.openai.com`
- **THEN** the forwarding target is `https://api.openai.com/v1/chat/completions?stream=true` and HTTP `Host` is `api.openai.com`

#### Scenario: URL joining after rewriting

- **WHEN** the request is `POST /api/chat/completions?next=/api&x=a%2Fb&x=2`, the selected rule is `/api:/v1`, and `base_url` is `https://api.openai.com`
- **THEN** the forwarding target is `https://api.openai.com/v1/chat/completions?next=/api&x=a%2Fb&x=2`, preserving query bytes and order

#### Scenario: Only configured transformations are applied

- **WHEN** a request carrying `Bearer sk-virt-dev` is forwarded
- **THEN** the upstream receives the selected target's authorization and authority, any configured path and model-name rewrites, and otherwise unchanged application-layer headers and body; no forwarding headers are added, and only standard HTTP hop-by-hop handling is allowed

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
