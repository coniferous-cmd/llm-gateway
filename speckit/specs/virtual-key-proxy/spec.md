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

The system SHALL load the virtual key and target mappings from the YAML configuration file once at startup and SHALL NOT reload the configuration while running (no hot reload). Each entry declares one `token`, one `target` (a bare-origin `base_url` plus an optional `api_key`), and MAY declare `models` (a list of `{id, name}`); the same `token` may appear multiple times in the configuration, and the system SHALL resolve it into an ordered entry list for that token, each entry carrying its own target and model declarations.

#### Scenario: One token with multiple targets

- **WHEN** `sk-virt-dev` appears three times in the config file with `base_url` values for OpenAI, DeepSeek, and local Ollama respectively
- **THEN** after startup, requests authenticated with `sk-virt-dev` can be routed to any of those three addresses

#### Scenario: base_url normalization

- **WHEN** `base_url` in the config is written as `http://127.0.0.1:11434/` (with a trailing slash) or without one
- **THEN** the joined target address is `http://127.0.0.1:11434` + the original request path in both cases, with no duplicated or missing `/`

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

The system SHALL forward requests to the address formed by joining `base_url` with the original path (including the query string), and SHALL set the HTTP `Host` to the selected target's authority (hostname plus explicit port); beyond that, only the `Authorization` header is replaced with the target's `api_key` and, when model-based routing matched a declaration with `id != name`, the top-level `model` field of a JSON object body is rewritten per the model name rewrite requirement. All other application-layer request headers and body content SHALL be forwarded as-is, with no forwarding headers such as `X-Forwarded-For` added or appended. Hop-by-hop headers may be removed per standard HTTP proxy rules (including headers named by `Connection`). The provider's response (status code, response headers, and SSE response body) SHALL be passed back to the caller as-is; streaming responses SHALL be forwarded chunk by chunk, not buffered until the response ends.

#### Scenario: URL joining

- **WHEN** the authenticated request is `POST /v1/chat/completions?stream=true` and the selected target's `base_url` is `https://api.openai.com`
- **THEN** the forwarding target is `https://api.openai.com/v1/chat/completions?stream=true` and the HTTP `Host` is `api.openai.com`, not the address the caller used to reach the gateway

#### Scenario: Only the Authorization header is replaced

- **WHEN** a request carrying `Bearer sk-virt-dev` is forwarded
- **THEN** the upstream receives `Authorization` set to the selected target's real `api_key`, the HTTP `Host` set to the selected target's authority, and the request body bytes and remaining application-layer headers unchanged; no forwarding headers are added, and only standard HTTP hop-by-hop header handling is allowed

#### Scenario: Streaming response passthrough

- **WHEN** the provider returns an SSE streaming response (`text/event-stream`)
- **THEN** the response body is passed back to the caller chunk by chunk, without being buffered or rewritten by the gateway

### Requirement: Errors relayed as-is

When the target provider is unreachable or returns an HTTP error status, the system SHALL relay the error status (and response body) to the caller as-is; the system SHALL NOT retry and SHALL NOT switch to another target of the same key (no failover).

#### Scenario: Provider unreachable

- **WHEN** the selected target address refuses the connection or times out
- **THEN** the caller receives an error response indicating upstream connection failure (e.g. `502 Bad Gateway`), and the gateway does not retry other targets

#### Scenario: Provider returns an error status

- **WHEN** the provider returns `429 Too Many Requests` with its original response body
- **THEN** the caller receives `429` and the same response body
