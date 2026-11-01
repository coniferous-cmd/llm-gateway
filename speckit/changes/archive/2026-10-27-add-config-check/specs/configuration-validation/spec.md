---
create-time: 2026-10-27 09:43:46
---

## ADDED Requirements

### Requirement: Offline configuration check command

The gateway SHALL accept `--check-config` with an optional `--config` path, defaulting to the existing default configuration path. It SHALL use the same configuration validation as normal startup, print a success message to stdout and exit 0 on success, or print an actionable error to stderr and exit 1 on failure. Diagnostics SHALL preserve available YAML line information and SHALL NOT dump the configuration or secret values. Reporting the first error is sufficient.

#### Scenario: Explicit file is valid

- **WHEN** `llm-gateway --check-config --config ./config.yaml` reads a valid file
- **THEN** it reports success and exits 0

#### Scenario: Default path

- **WHEN** `--check-config` is provided without `--config`
- **THEN** the gateway checks `~/.config/llm-gateway/config.yaml` using the existing home-directory resolution

#### Scenario: Invalid or unreadable file

- **WHEN** the file cannot be read, has malformed YAML, or violates any configuration rule
- **THEN** checking exits 1 with the reason and available field/line context, without reporting success

### Requirement: Side-effect-free checking

Configuration checking SHALL NOT start the gateway, bind ports, contact upstreams or peers, create conversation-log directories, read or write PID files, open log files, or launch child processes. It SHALL NOT assert that ports, credentials, remote services, or output directories are usable at runtime.

#### Scenario: Runtime resources unavailable

- **WHEN** a valid configuration uses a listener port already occupied and an unreachable upstream, with `--log-file` and `--pid-file` pointing at nonexistent paths
- **THEN** checking succeeds without network activity or creating those files or conversation-log directories

### Requirement: Check operation conflicts

The gateway SHALL reject `--check-config` combined with any of `--daemon`, `--stop`, `--keygen`, or `--version`, report the conflict to stderr, and exit 1 before performing either operation. `--log-file` and `--pid-file` MAY be supplied but SHALL have no effect during checking.

#### Scenario: Conflicting operation

- **WHEN** `--check-config` is combined with another operation flag
- **THEN** the command reports the incompatible flags and exits 1 without side effects

### Requirement: Strict known configuration fields

Both startup and checking SHALL reject unknown YAML mapping fields at every supported configuration level, including root, keys, targets, models, rewrite rules, E2EE, peers, and conversation logging. Existing supported fields, defaults, repeated virtual tokens, and valid YAML aliases SHALL remain supported. Unknown-field errors SHALL identify the field and its line without echoing its value.

#### Scenario: Mistyped nested field

- **WHEN** a target contains `base_urll` rather than `base_url`
- **THEN** both checking and startup fail with an unknown-field diagnostic naming `base_urll` and its line

#### Scenario: Valid documented configuration

- **WHEN** the documented example configuration is checked
- **THEN** all supported fields are recognized and validation succeeds

### Requirement: Listener address syntax validation

The shared configuration loader SHALL validate effective `listen` and configured `e2ee.listen` as host-and-numeric-port addresses without DNS resolution or binding. Empty configured strings SHALL retain existing defaults. An empty host, hostname, IPv4 address, or bracketed IPv6 address SHALL be accepted with an integer port from 0 through 65535. Missing ports, nonnumeric ports, out-of-range ports, malformed IPv6, and whitespace-corrupted addresses SHALL be rejected with field and available line context.

#### Scenario: Valid listener forms

- **WHEN** a listener is `:8080`, `localhost:8080`, `127.0.0.1:0`, or `[::1]:8080`
- **THEN** syntax validation accepts it without looking up or binding the host

#### Scenario: Malformed listener

- **WHEN** either listener uses `localhost`, `:abc`, `:65536`, or unbracketed IPv6 with a port
- **THEN** checking and startup reject it with the relevant listener field and available line information
