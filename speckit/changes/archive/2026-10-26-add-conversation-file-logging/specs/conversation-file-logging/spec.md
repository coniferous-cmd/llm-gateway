---
create-time: 2026-10-26 17:18:45
---

## ADDED Requirements

### Requirement: Opt-in configuration

The system SHALL support a startup-only `conversation_log` mapping containing `enabled` (default false), `directory` (default `logs/conversations`, relative to the configuration file directory), and `max_body_bytes` (default 8388608, independently applied to each body). Explicit limits SHALL be positive integers. Invalid field types, blank explicit directories, or non-positive explicit limits SHALL fail configuration validation. Disabled logging SHALL create no conversation directories or files and SHALL not install body capture. Existing configuration files SHALL remain valid.

#### Scenario: Existing configuration

- **WHEN** the configuration omits `conversation_log`
- **THEN** forwarding and request summaries remain unchanged and no conversation files are created

#### Scenario: Enabled defaults

- **WHEN** `conversation_log.enabled` is true and other settings are omitted
- **THEN** the gateway uses `logs/conversations` beside the configuration file and captures at most 8388608 bytes of each request and response

#### Scenario: Invalid explicit limit

- **WHEN** `max_body_bytes` is explicitly zero, negative, or not an integer
- **THEN** startup fails with an error identifying the configuration field

### Requirement: One record per authenticated call

With logging enabled and storage available, the system SHALL write exactly one JSON file when each authenticated HTTP handler finishes, including preparation failures and upstream errors. It SHALL generate an independent collision-resistant request ID for each call, use the UTC start date and time for `<directory>/YYYY-MM-DD/HHmmss_<request-id>.json`, and never overwrite another call's record. It SHALL apply equally to the ordinary HTTP listener and requests decrypted by the encrypted listener. Unauthorized requests SHALL retain summary logging only and SHALL NOT have their bodies captured for conversation logging. Final files SHALL be published only after complete JSON serialization; process crashes before finalization are not guaranteed to produce a record.

#### Scenario: Concurrent calls

- **WHEN** multiple authenticated requests start in the same second and complete
- **THEN** each receives a distinct file containing only its own request and response

#### Scenario: Authentication or preparation failure

- **WHEN** one request fails authentication and another authenticated request fails model preparation
- **THEN** only the authenticated request produces a conversation file, with its rejection status and available captured content

#### Scenario: Shutdown with an active encrypted call

- **WHEN** graceful shutdown begins while the ordinary listener is idle and the encrypted listener is serving an authenticated streaming call
- **THEN** the process waits for both listener lifecycles to finish within their existing drain budgets, allowing the encrypted call to finalize its conversation file before normal process exit

### Requirement: Conversation record content

Each record SHALL include a schema version, request ID, UTC start/end timestamps, duration in milliseconds, method, original escaped path without query, selected upstream origin and matched model ID/name when available, final HTTP status when available, response completion state, and a sanitized error category when applicable. It SHALL preserve original request bytes observed before path/model rewriting and response bytes successfully written through the gateway response writer. Bodies SHALL be represented as UTF-8 text when valid and base64 otherwise, with an explicit encoding field, observed byte count, captured byte count, truncation flag, and completeness flag. Empty bodies SHALL be represented explicitly. Raw SSE SHALL remain raw SSE, without requiring provider-specific parsing.

#### Scenario: Model alias

- **WHEN** a request uses an alias which is rewritten to an upstream model name
- **THEN** the request body in the record contains the original alias and the model metadata identifies both names

#### Scenario: Encoded or non-JSON body

- **WHEN** a call carries compressed or binary body bytes
- **THEN** the captured bytes round-trip through the declared text/base64 representation without decompression or protocol-specific interpretation

### Requirement: Bounded transparent capture

Capture SHALL retain no more than `max_body_bytes` per body, continue normal forwarding after that limit, and mark truncation rather than rejecting the call. It SHALL observe reads and writes without eagerly draining request bodies or waiting for a full response before forwarding. It SHALL preserve response flushing, status/header behavior, existing routing, and cancellation. An unconsumed or partially read request SHALL be marked incomplete rather than drained solely for logging. A response read/write failure or client cancellation SHALL mark the response incomplete, even when HTTP headers already committed status 200. HTTP error status alone SHALL NOT imply an incomplete response.

#### Scenario: Streaming before completion

- **WHEN** an upstream flushes an SSE chunk and waits before emitting the remainder
- **THEN** the client receives that chunk before the upstream finishes, and the finalized record contains the captured stream bytes in order

#### Scenario: Capture limit exceeded

- **WHEN** a body exceeds its configured capture limit
- **THEN** forwarding continues unchanged, the retained prefix is bounded, and the record identifies truncation independently of transfer completeness

#### Scenario: Mid-stream disconnect

- **WHEN** the upstream disconnects unexpectedly after response headers or the client cancels
- **THEN** finalization is attempted with partial content and an incomplete response state, without inventing a successful complete reply

### Requirement: Sensitive data and persistence failure isolation

The system SHALL NOT serialize request/response headers, virtual tokens, upstream API keys, encryption private keys, raw query strings, or raw transport errors into structured conversation metadata or persistence-error diagnostics. Body content SHALL NOT be automatically redacted; documentation SHALL warn that it may contain secrets. Newly created directories and files SHALL request owner-only permissions (0700/0600 where supported); Windows deployments SHALL be instructed to restrict ACLs. Storage initialization or write failures SHALL emit safe operational error diagnostics and SHALL NOT change client status, response bytes, routing, or retry behavior. Existing request summaries SHALL remain available and include the generated request ID for captured calls.

#### Scenario: Storage unavailable

- **WHEN** the configured directory cannot be created or a record cannot be written
- **THEN** the gateway reports the logging failure and continues serving calls without replacing responses with logging errors

#### Scenario: Credentials outside bodies

- **WHEN** a request has credentials in headers or query parameters and uses a configured upstream key
- **THEN** conversation metadata and persistence-error diagnostics do not contain those credentials
