---
create-time: 2026-11-09 09:47:43
---

## MODIFIED Requirements

### Requirement: Conversation record content

Each record SHALL include a schema version, request ID, UTC start/end timestamps, duration in milliseconds, method, original escaped path without query, selected upstream origin and matched model ID/name when available, final HTTP status when available, response completion state, and a sanitized error category when applicable. It SHALL preserve original request bytes observed before path/model rewriting and the decoded response bytes successfully written through the gateway response writer. When the gateway decodes a content-coded provider response before delivery, capture SHALL observe the decoded plaintext bytes that the caller receives; the record SHALL NOT contain the provider's encoded form, and the observed byte count SHALL count decoded bytes. Request bodies SHALL NOT be decoded for capture. Bodies SHALL be represented as UTF-8 text when valid and base64 otherwise, with an explicit encoding field, observed byte count, captured byte count, truncation flag, and completeness flag. Empty bodies SHALL be represented explicitly. Raw SSE SHALL remain raw SSE, without requiring provider-specific parsing.

#### Scenario: Model alias

- **WHEN** a request uses an alias which is rewritten to an upstream model name
- **THEN** the request body in the record contains the original alias and the model metadata identifies both names

#### Scenario: Compressed request body

- **WHEN** the request body arrives with a non-identity `Content-Encoding`
- **THEN** the captured request bytes round-trip through the declared text/base64 representation without decompression or protocol-specific interpretation

#### Scenario: Decoded provider response

- **WHEN** the provider returns a gzip-, deflate-, brotli-, or zstd-encoded response and the gateway delivers the decoded body
- **THEN** the record contains the decoded plaintext bytes and reports observed and captured counts for the decoded form, not the encoded form

#### Scenario: Binary payload

- **WHEN** a captured request or response payload is not valid UTF-8
- **THEN** it is represented as base64 with the declared encoding, including when the gateway decoded a content coding before capture

### Requirement: Bounded transparent capture

Capture SHALL retain no more than `max_body_bytes` per body, continue normal forwarding after that limit, and mark truncation rather than rejecting the call. It SHALL observe reads and writes without eagerly draining request bodies or waiting for a full response before forwarding. It SHALL preserve response flushing, status/header behavior, existing routing, and cancellation. An unconsumed or partially read request SHALL be marked incomplete rather than drained solely for logging. A response read/write failure, a content-decoding failure, or client cancellation SHALL mark the response incomplete, even when HTTP headers already committed status 200. HTTP error status alone SHALL NOT imply an incomplete response.

#### Scenario: Streaming before completion

- **WHEN** an upstream flushes an SSE chunk and waits before emitting the remainder
- **THEN** the client receives that chunk before the upstream finishes, and the finalized record contains the captured decoded stream bytes in order

#### Scenario: Capture limit exceeded

- **WHEN** a body exceeds its configured capture limit
- **THEN** forwarding continues unchanged, the retained prefix is bounded, and the record identifies truncation independently of transfer completeness

#### Scenario: Mid-stream disconnect

- **WHEN** the upstream disconnects unexpectedly after response headers or the client cancels
- **THEN** finalization is attempted with partial content and an incomplete response state, without inventing a successful complete reply

#### Scenario: Decode failure

- **WHEN** a content-coded response body is truncated or corrupt after headers have been committed
- **THEN** the record contains the decoded prefix that was delivered, marks the response incomplete, and carries the content-decoding error category
