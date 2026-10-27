---
create-time: 2026-10-27 09:43:19
---

# Proposal

## Why

Operators need to check configuration before deployment without starting a gateway or contacting providers. Startup already validates most configuration, but silently ignored unknown fields and unchecked listener address syntax can hide mistakes.

## What Changes

- Add `--check-config`, using `--config` or the existing default configuration path, with exit code 0 on success and 1 on failure.
- Reuse startup configuration validation without opening listeners, writing PID/log files, starting background processes, or contacting upstreams.
- Reject unknown YAML fields at every configuration level and validate plaintext/E2EE listener address syntax in the shared loader.
- Define conflicting operation flags, actionable diagnostics, tests, and user documentation.
- **Compatibility:** configuration with previously ignored unknown fields or malformed listener addresses will fail during normal startup as well as explicit checking.

## Impact

- Affected specs: new `configuration-validation` capability, complementing existing configuration loading in `virtual-key-proxy`, `gateway-e2ee`, and `conversation-file-logging`.
- Affected code: `cmd/llm-gateway/main.go`, CLI integration tests, `internal/config` parser and tests, README and CLI help.
- No upstream connectivity tests, runtime resource availability guarantees, new dependencies, or configuration migration tools.
