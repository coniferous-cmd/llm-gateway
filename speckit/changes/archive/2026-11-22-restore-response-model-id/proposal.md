---
create-time: 2026-11-22 10:11:42
---

# Proposal

## Why

Clients select models using the gateway's configured IDs, but completion responses currently expose the provider's model name. Several IDs can route to the same upstream model, so the client needs the ID selected for its own call to identify the model consistently across requests and responses.

## What Changes

- Restore the model ID resolved for the current request in existing model fields of successful OpenAI Chat Completions and Anthropic Messages responses.
- Inspect non-streaming JSON after the decoded body ends, with a 32 MiB inspection limit. Responses above that limit are forwarded without rewriting. SSE delivery remains incremental.
- Add nested JSON field scanning for Anthropic `message_start` events. Replace only the `message.model` string literal and preserve SSE framing and surrounding bytes.
- Limit SSE inspection to 1 MiB per decoded event. Forward oversized events as they arrive and flush any buffered event bytes unchanged before EOF or a body error.
- Use the model ID exactly as resolved, including any `[1m]` suffix. Each concurrent request carries its own resolved ID through to its response.
- Keep transparent forwarding for requests handled without a matched model declaration and for provider errors.

## Impact

- Affected spec: `virtual-key-proxy`.
- Affected code: request preparation, response forwarding, JSON model scanning, model scanner tests, HTTP integration tests, and model-routing documentation.
