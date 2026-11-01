---
create-time: 2026-10-27 09:43:46
---

# Design

## Context

`cmd/llm-gateway/main.go` uses standard-library flags. Key generation and version currently return before startup, while PID resolution, daemon handling, logging, and server setup follow. `internal/config.Load` reads a file and delegates to node-based `Parse`, which performs semantic checks and applies defaults. Several sections use custom YAML-node parsing, so simply enabling decoder KnownFields on the outer struct would not cover all fields and could reject valid custom sections.

## Decisions

- Add a `--check-config` branch immediately after flag parsing and conflict validation, before keygen/version and all process/resource setup. Resolve the same configuration default and call `config.Load`. Print a concise success line to stdout; print failures to stderr and exit 1. Keep existing normal-startup logging unchanged.
- Reject check combined with daemon, stop, keygen, or version before either operation runs. Treat log-file and pid-file as ignored startup options in check mode, allowing deployment scripts to reuse arguments without touching those resources.
- Keep syntax and semantic checks in the shared config package, not a parallel checker. Existing E2EE, model, rewrite, target, and conversation-log checks remain authoritative. Do not call runtime builders, bind sockets, resolve DNS, or probe credentials.
- Validate known fields with a schema-aware YAML-node traversal covering every supported mapping, including custom-parsed sections. Preserve alias handling and supported YAML merge behavior, guard traversal against cycles, and report unknown field names and node lines without values. Inventory the existing parser before implementing; do not infer allowed fields from the outer raw struct alone.
- Validate effective listener values after defaults using `net.SplitHostPort`, numeric port range checking (0–65535), and host syntax checks without resolution. Allow empty hosts, ordinary hostnames, IPv4, and bracketed IPv6; reject whitespace corruption and malformed brackets/IP literals. Address availability and hostname resolution remain runtime concerns.
- Preserve existing semantic diagnostics, except redact secret virtual token values in model-duplicate errors reachable from the shared loader. Do not serialize parsed configuration or include private keys/API keys/tokens in new diagnostics. First-error reporting keeps scope bounded.
- Add config table tests and real-binary CLI tests. Prove a check succeeds with an occupied listener and unreachable upstream, and leaves supplied PID/log paths and conversation directories untouched. Test conflicts before early-exit operations to prevent key generation or version output masking the check.

## Risks / Trade-offs

- Strictness intentionally breaks previously tolerated typos. Document that startup now rejects unknown fields and malformed listeners; retain all valid documented configurations and defaults.
- A manually maintained known-field schema can drift. Cover each nesting level, custom sections, aliases/merges, and the example configuration in regression tests.
- Check success is not a readiness guarantee: port availability, DNS, upstream authentication, filesystem permissions, and remote service health are explicitly excluded.
- Port 0 remains valid for existing ephemeral-listener usage; named service ports are rejected in favor of deterministic offline validation.
