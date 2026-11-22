---
create-time: 2026-11-22 10:11:58
---

# Design

## Context

Request preparation resolves the configured model before forwarding. The server keeps that model in the request state while awaiting the provider response. Preparation also records a synthetic model for logging on some passthrough requests, so response rewriting needs to distinguish a matched declaration from that logging metadata. The response body goes through content decoding before it reaches the observer used for conversation capture.

## Decisions

- Carry the resolved ID from the matched declaration through the same HTTP handler into its response body. Each request owns its ID, which handles concurrent requests and providers that return a different model spelling. The existing route resolution supplies the complete ID, including any suffix.
- Select the response format using the forwarded endpoint and response media type. Use the same path resolution as the forwarder so aliases such as `/anthropic` work. Gate rewriting on a successful response with a body and a matched declaration.
- Target the top-level `model` in ordinary JSON responses and OpenAI Chat Completions SSE chunks. Target `message.model` in the Anthropic `message_start` payload. Replace existing string values and preserve unrelated data and SSE framing. Carry partial events across body chunks and emit each complete event without waiting for the rest of the stream.
- Apply the transformation after content decoding and before observation so conversation capture records the bytes sent to the client. Adjust length and integrity headers and ensure byte accounting uses the outgoing body. Preserve existing upstream read and decode error handling.

### Non-streaming JSON body reads

Use a lazy body wrapper between decoding and `ObservedBody`. Return response headers without waiting for upstream EOF. Skip the wrapper when a known unencoded `Content-Length` exceeds 32 MiB. For other candidates, remove the original length and integrity headers before sending headers and leave the expected response byte count unknown so observation completes on EOF.

1. Accumulate decoded data frames up to 32 MiB. At exactly 32 MiB, keep polling for clean EOF, more data, or an error. Retain any trailers for forwarding after the data on clean completion.
2. If the next data frame would exceed the limit, replay the buffered bytes and the entire crossing frame in order, then forward remaining frames directly. Keep the crossing frame as its original `Bytes` to avoid copying it into the inspection buffer. Do not wait for EOF in this case.
3. On clean EOF within the limit, validate the entire JSON document, then call `model_value()` once on valid input. `None` covers both incomplete input and a missing string field, and `Some` can be returned before the closing delimiter, so a scanner result cannot determine whether to keep reading. Splice the existing string if present. Emit the original decoded bytes for invalid JSON, an absent or non-string field, or an already matching value.
4. On an upstream read or decode error, emit any buffered original bytes followed by the original body error. `ObservedBody` records the response as incomplete under the existing error category. The HTTP/1 transport delays delivering the error until pending bytes are flushed.

### SSE event buffering and termination

For SSE eligible for rewriting, buffer at most 1 MiB (1,048,576 bytes) of decoded input for the current event, including SSE field prefixes, line endings, and the terminating blank line. Recognize LF, CRLF, and CR as line endings and carry delimiter state across frames, including a split CRLF. A complete event of exactly 1 MiB is still eligible for rewriting and is emitted as soon as its delimiter is recognized.

- When the event would exceed the cap, emit its buffered prefix and stream the rest of that event through unchanged as bytes arrive, even if a delimiter never arrives. Keep only delimiter state while bypassing. Resume inspection immediately after the next blank line, including when the next event starts in the same body frame. Count only bytes belonging to the current event and avoid copying an entire incoming frame into the event buffer.
- On clean EOF, emit any remaining unterminated event bytes unchanged, then return EOF. On an upstream read or decode error, emit any remaining unterminated event bytes unchanged before returning the original body error. Drain these bytes exactly once through `ObservedBody` and preserve the upstream error classification. The flush adds no delimiter and never attempts to rewrite a partial event.

### HTTP/1 write ordering on body failure

Hyper can poll a body error before writing the data frames it already accepted. For an inspected response, preserve the original error after observation and pause polling the outgoing body until the connection reports a successful transport flush. Hyper then aborts the body using the same error. The flush must cover pending response bytes, including any buffered JSON prefix or SSE event. If the transport write is backpressured, wait for it to become writable and finish flushing. If the downstream connection fails, end the connection normally for a transport failure. This applies to both plain and encrypted gateway listeners, which share the same HTTP/1 service. Clean responses continue to use ordinary HTTP framing.

### Anthropic `message_start` model field

Add a nested lookup to `src/proxy/model.rs` for the exact path `message.model`. Keep `model_value()` and its top-level behavior for request routing, ordinary JSON responses, and OpenAI SSE. Reuse the scanner's object traversal and string parsing to match decoded key names and return a `ModelSpan` relative to the complete JSON payload. Match the first top-level `message` member and the first `model` member directly inside it. The `message` value must be an object and the `model` value must be a string. A missing member or wrong type leaves the event unchanged.

For a completed `event: message_start`, read the event's `data:` fields in order and join their payloads with LF for JSON validation and lookup. Track which original byte range supplied each payload segment. A valid JSON string literal cannot span an inserted LF, so map the returned span back to its original `data:` line and pass that span to the existing byte splice and `go_json_quote` escaping. Preserve every other event byte, including comments, field prefixes, line endings, other JSON literals, and the event delimiter. Forward malformed JSON and unrelated events as received.

## Risks / Trade-offs

- Clients that read provider model names or version suffixes from these responses will now see the gateway's selected ID. The model name sent upstream remains available in operational model metadata.
- An SSE event above the inspection cap can expose the provider model name. Inspection resumes for the next event.
- The client receives eligible non-streaming JSON body bytes within the inspection limit after EOF. Concurrent responses can each hold up to 32 MiB of decoded data for inspection, plus the temporary allocation used to rewrite the model string. Over-limit responses can expose the provider model name.
