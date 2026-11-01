---
create-time: 2026-10-27 09:45:04
---

## 1. Shared configuration validation

- [x] 1.1 Inventory all supported YAML mapping fields, including custom conversation-log and rewrite parsers; implement strict known-field validation with field/line diagnostics, alias handling, and cycle protection. Add table tests for unknown fields at every nesting level and valid aliases/merges already supported by the parser.
- [x] 1.2 Validate defaulted plaintext and E2EE listener syntax without DNS or socket operations. Test empty defaults, empty hosts, hostnames, IPv4, bracketed IPv6, port 0, range boundaries, missing/nonnumeric ports, invalid brackets, and whitespace.
- [x] 1.3 Audit diagnostics reachable from config.Load for secret disclosure, including duplicate-model token reporting; redact values while retaining actionable field/index/line information and add regression tests.

## 2. CLI command

- [x] 2.1 Add --check-config help and early execution using existing explicit/default configuration path resolution and config.Load; return success on stdout with exit 0 or failure on stderr with exit 1 before startup resources are touched.
- [x] 2.2 Reject check combined with daemon, stop, keygen, or version before their existing early exits; ignore log-file and pid-file during checking. Cover every conflict in real-binary CLI tests.
- [x] 2.3 Add real-binary tests for explicit/default paths, missing files, malformed YAML, semantic errors, strict-field errors, listener errors, stdout/stderr separation, and prompt exit.
- [x] 2.4 Verify checking succeeds with an occupied port and unreachable upstream, performs no upstream/peer requests, and does not create/open PID/log files or conversation directories. Verify startup uses the same strict validation.

## 3. Documentation and verification

- [x] 3.1 Document check examples, default path, exit statuses, conflicts, side-effect boundaries, and stricter startup compatibility in README and CLI help.
- [x] 3.2 Run gofmt on changed Go files, go test ./..., and go vet ./...; check config.example.yaml with the new command and confirm existing startup, daemon, E2EE, and conversation-log tests remain passing. Record platform limitations or failures rather than assuming success.

## Verification results

- Platform: Windows/amd64. Linux and macOS were not executed in this session.
- Applied gofmt to all modified/new Go files.
- `go test ./internal/config`: 145 tests passed.
- `go test ./cmd/llm-gateway -run TestCLICheckConfig -count=1`: 14 tests passed.
- `go test ./...`: 372 tests passed across 8 packages, including existing startup, daemon, E2EE, and conversation-log coverage.
- `go vet ./...`: passed.
- `go run ./cmd/llm-gateway --check-config --config ./config.example.yaml`: passed; printed `configuration valid: ./config.example.yaml`.
- `git diff --check`: passed.
