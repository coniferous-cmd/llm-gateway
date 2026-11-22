---
create-time: 2026-11-22 10:12:40
---

## 1. Implementation

- [x] 1.1 Carry the resolved ID from a matched declaration into its own response handling and use the forwarded path to identify eligible endpoints. Verify that passthrough-only request metadata cannot trigger rewriting.
- [x] 1.2 Use the existing top-level model span and splice for ordinary JSON and OpenAI SSE. Preserve all bytes outside the replaced string and forward unrelated events, sentinels, and payloads without a string model field.
- [x] 1.3 Connect the rewrite after decoding and before conversation capture. Correct stale length and integrity metadata. After observation, hold the original error for an inspected HTTP/1 response until the downstream transport flushes pending writes, including under backpressure.
- [x] 1.4 Add a lazy non-streaming JSON body wrapper. Read decoded frames up to 32 MiB, validate on clean EOF, and scan once. Bypass known oversized unencoded bodies. On overflow, replay the buffered prefix and crossing frame before passing through the rest. On a read or decode error, replay the original prefix and propagate the error.
- [x] 1.5 Update the model-routing documentation with the exact resolved ID contract and the JSON and SSE inspection limits and fallback behavior.
- [x] 1.6 Extend `src/proxy/model.rs` with an exact `message.model` lookup that returns the string literal's span in the entire JSON payload. Keep the current top-level lookup behavior.
- [x] 1.7 Implement Anthropic SSE rewriting for `event: message_start`. Validate the joined `data:` payload, map the nested span back to the original event, and splice with `go_json_quote` semantics while retaining the surrounding bytes and framing.
- [x] 1.8 Bound SSE inspection to 1 MiB per decoded event, including its delimiter. On overflow, drain the buffered bytes and relay the rest of that event with only delimiter state, then resume inspection after its delimiter. Drain an unterminated event unchanged exactly once before EOF or the original upstream read or decode error.

## 2. Verification

- [x] 2.1 Cover JSON and SSE for both protocols, including an upstream versioned model name, same-name mappings, `[1m]` IDs, and model fields that are absent or non-string.
- [x] 2.2 Cover two aliases mapped to the same provider model with overlapping requests and reversed response order.
- [x] 2.3 Verify that split and coalesced SSE events are delivered before the upstream stream ends, including with compression. Verify response headers and conversation capture against the body sent to the client. Retain the existing Anthropic passthrough byte-for-byte and first-event delivery assertions.
- [x] 2.4 Cover JSON with a split `model` key or value in identity and compressed responses. Verify exact-limit responses are rewritten only after clean EOF, known oversized bodies bypass inspection, and overflow during reading replays bytes in order and resumes forwarding before EOF. Include a compressed body whose decoded size exceeds the limit.
- [x] 2.5 Verify clean EOF with invalid JSON, including a complete model string followed by a truncated object, passes through unchanged. Verify upstream read and decode failures remain body errors and are recorded as incomplete during inspection.
- [x] 2.6 Unit-test nested lookup and byte splicing with escaped key names, Unicode, quotes, control characters, and Go HTML escapes. Cover first-member behavior, unrelated `model` fields at other paths, and missing or wrong-typed members. Assert byte equality outside the replaced literal and retain the current top-level scanner tests.
- [x] 2.7 Verify Anthropic SSE with multiple `data:` lines, comments, CRLF, and an event split across body frames. Compare the entire output with the expected event bytes and verify malformed JSON and other event types pass through unchanged.
- [x] 2.8 Verify provider errors, unmatched requests, and unrelated endpoints retain their forwarding behavior. Run the HTTP integration tests and the Rust test suite.
- [x] 2.9 Verify a complete SSE event at exactly the cap can be rewritten and an event one byte over passes through unchanged. Hold the upstream open without a delimiter after exceeding the cap and verify the client receives the exact prefix before releasing the upstream. Then deliver its delimiter and a rewriteable event in the same frame and verify inspection resumes, including with a CRLF split across frames and a compressed response whose decoded event exceeds the cap.
- [x] 2.10 Verify an SSE stream ending with a partial event forwards the original bytes exactly once before EOF. Repeat with upstream read and decode failures, asserting the partial bytes precede the original error and conversation capture records an incomplete response. Verify the HTTP/1 transport still delivers data ahead of the error when downstream writing is backpressured. Cover a valid JSON payload lacking the SSE blank-line delimiter to ensure it is forwarded unchanged.
