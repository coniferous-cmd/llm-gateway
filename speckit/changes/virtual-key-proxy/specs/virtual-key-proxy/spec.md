---
create-time: 2026-10-20 14:44:58
---

## ADDED Requirements

### Requirement: Virtual key authentication

The system SHALL accept HTTP requests carrying an `Authorization: Bearer <token>` header and look the token up in the configuration loaded at startup. On a hit the request proceeds; on a miss the system returns `401 Unauthorized`. The system SHALL NOT perform any validation beyond virtual key authentication (no restrictions on path, HTTP method, Content-Type, or request body content).

#### Scenario: Valid virtual key

- **WHEN** a request carries `Authorization: Bearer sk-virt-dev` and that token exists in the configuration
- **THEN** the request is accepted and enters the forwarding flow, and is not rejected because of its path or method

#### Scenario: Invalid or missing virtual key

- **WHEN** a request lacks the `Authorization` header, its format is not `Bearer <token>`, or the token is not in the configuration
- **THEN** the system returns `401 Unauthorized` and forwards the request to no provider

### Requirement: Configuration file loading

The system SHALL load the virtual key and target mappings from the YAML configuration file once at startup and SHALL NOT reload the configuration while running (no hot reload). Each entry declares one `token` and one `target` (a bare-origin `base_url` plus an optional `api_key`); the same `token` may appear multiple times in the configuration, and the system SHALL resolve it into a target list for that token.

#### Scenario: One token with multiple targets

- **WHEN** `sk-virt-dev` appears three times in the config file with `base_url` values for OpenAI, DeepSeek, and local Ollama respectively
- **THEN** after startup, requests authenticated with `sk-virt-dev` can be routed to any of those three addresses

#### Scenario: base_url normalization

- **WHEN** `base_url` in the config is written as `http://127.0.0.1:11434/` (with a trailing slash) or without one
- **THEN** the joined target address is `http://127.0.0.1:11434` + the original request path in both cases, with no duplicated or missing `/`

### Requirement: Round-robin routing

When one virtual key maps to multiple targets, the system SHALL select a target for each authenticated request in round-robin order (atomic counter modulo), distributing requests evenly across targets, and the routing behavior SHALL remain race-free under concurrent requests.

#### Scenario: Round-robin across multiple targets

- **WHEN** `sk-virt-dev` is configured with 3 targets and 6 authenticated requests arrive in sequence
- **THEN** the 6 requests hit targets 1, 2, 3, 1, 2, 3 in order (each target selected exactly twice)

#### Scenario: Single target

- **WHEN** the virtual key is configured with only 1 target
- **THEN** all authenticated requests are forwarded to that target

### Requirement: Transparent forwarding

The system SHALL forward requests to the address formed by joining `base_url` with the original request path (including the query string), and SHALL set the HTTP `Host` to the selected target's authority (hostname plus explicit port); beyond that, only the `Authorization` header is replaced with the target's `api_key`, while the request body and remaining application-layer request headers SHALL be forwarded as-is, with no forwarding headers such as `X-Forwarded-For` added or appended. Hop-by-hop headers may be removed per standard HTTP proxy rules (including headers named by `Connection`). The provider's response (status code, response headers, and SSE response body) SHALL be passed back to the caller as-is; streaming responses SHALL be forwarded chunk by chunk, not buffered until the response ends.

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
