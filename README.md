# llm-gateway

A lightweight, locally running LLM gateway: callers only store virtual keys; the gateway picks the upstream, swaps `Authorization`, and optionally [rewrites path prefixes](#path-rewriting) per target. It does not convert protocols, count tokens, retry, fail over, or hot-reload configuration. Request bodies pass through untouched, with one optional exception: [model routing](#model-routing) reads — and possibly rewrites — the top-level `model` field of JSON request bodies.

Supports Windows, Linux, macOS (amd64 / arm64). The release is a single binary with no dependency on Go, Node.js, Python, or a C compiler; the config file is supplied separately. The third-party Go dependencies are `gopkg.in/yaml.v3` (config parsing) and `github.com/flynn/noise` (the Noise protocol, used only for the optional gateway-to-gateway encrypted channel).

## Download

Prebuilt binaries for Windows, Linux and macOS (amd64 / arm64, plus Linux arm) are published on the [Releases](../../releases) page. Each archive contains the binary and `config.example.yaml`; a `SHA256SUMS` file is provided for verification. Check the binary's version with `llm-gateway --version`.

## Install and build

Building from source requires **Go 1.26.4 or newer**. GNU Make is optional; running and building the release binary does not require a C compiler.

```sh
go mod download
go build -trimpath -o bin/llm-gateway ./cmd/llm-gateway
```

Windows PowerShell:

```powershell
$env:CGO_ENABLED = '0'
go mod download
go build -trimpath -o bin/llm-gateway.exe ./cmd/llm-gateway
```

To explicitly disable CGO on Linux / macOS:

```sh
CGO_ENABLED=0 go build -trimpath -o bin/llm-gateway ./cmd/llm-gateway
```

With GNU Make, `make build` disables CGO by default and builds the native binary.

## Configuration

Copy `config.example.yaml` to `~/.config/llm-gateway/config.yaml` (or pass its location with `--config`), or create the following file:

```yaml
listen: "127.0.0.1:8080" # optional, this is the default address
keys:
  - token: sk-virt-dev
    target:
      base_url: https://api.openai.com
      api_key: sk-real-replace-me
  - token: sk-virt-local
    target:
      base_url: http://127.0.0.1:11434/
      # api_key is optional; when omitted the Authorization header is
      # removed so the virtual key is never leaked upstream
```

- `keys` must not be empty; each entry needs a non-empty `token` and a valid `target.base_url`.
- `base_url` must be an HTTP(S) **bare origin**, e.g. `https://api.openai.com`; it must not contain a path such as `/v1`, a query, a fragment, or username/password. A trailing `/` is normalized away.
- A non-empty `api_key` becomes `Authorization: Bearer <api_key>`; do not include `Bearer ` in the config value.
- A target may declare `rewrite`, an ordered list of path prefix rules — see [path rewriting](#path-rewriting). This field belongs inside `target`, not alongside `token`.
- An entry may declare `models` — the model names it serves — turning on [model routing](#model-routing) for that token.
- A target may declare `expire`, an timestamp after which the provider stops being used: RFC3339 (`'2026-11-01T00:00:00Z'`) or Unix seconds (`1800000000`). The check runs per request, so an entry expires without a restart; round-robin skips it, and a model whose only declaring entry expired answers `400 unknown model id`. A token whose every entry has expired authenticates as unknown (`401`). `0` or an omitted `expire` never expires.
- Declaring the same `token` again appends another target in config order; each authenticated request round-robins through them, with counters kept per token. Only put upstreams that are compatible in protocol and model names under the same token.
- The config is loaded once at startup; changes require a restart. Startup validation failures report the offending field and exit.
- The config contains plaintext secrets — never commit a real config; the default `.gitignore` already excludes `config.yaml`. Restrict file permissions and prefer listening on loopback only.

For example, two compatible upstreams for the same virtual key:

```yaml
keys:
  - token: sk-virt-dev
    target:
      base_url: http://127.0.0.1:11434
  - token: sk-virt-dev
    target:
      base_url: http://127.0.0.1:11435
```

### Path rewriting

Each target may map client path prefixes to the paths its upstream expects:

```yaml
keys:
  - token: sk-virt-dev
    target:
      base_url: https://api.openai.com
      api_key: sk-real-replace-me
      rewrite:
        - '/api:/v1'
        - '/openai:/v1'
  # The same token can select another upstream with its own rules.
  - token: sk-virt-dev
    target:
      base_url: https://other.example.com
      api_key: sk-other-replace-me
      rewrite:
        - '/api:/v2'
        - '/openai:/v2'
```

After model routing or round-robin selects an entry, **only that target's rules** apply. With the first target above, `/api/chat/completions?stream=true` forwards to `https://api.openai.com/v1/chat/completions?stream=true`; `/openai/models` forwards to `/v1/models`. Selecting the second target instead uses its `/v2` mappings. Upstreams under one token still need to speak compatible protocols and support the caller's models.

- `rewrite` is an optional **list of strings**, each `source:destination`; omitted or `[]` leaves paths unchanged. Invalid rules fail startup with the key index, field or rule index, and YAML line.
- Match only a complete path-segment prefix, case-sensitively: `/api` matches `/api` and `/api/models`, but not `/apix/models` or `/other/api/models`.
- **First matching rule wins**, not the longest match. Put more-specific prefixes before broader ones. Rewriting happens once per gateway hop: `['/api:/v1', '/v1:/v2']` turns `/api/models` into `/v1/models`, not `/v2/models`.
- Use destination `-` as the **delete-prefix marker**: `'/api:-'` maps `/api/v1/messages` to `/v1/messages`, `/api/api/models` to `/api/models`, and `/api` or `/api/` to `/`. It still uses full-segment matching and stops after the first matching rule. The marker is allowed only as the entire destination; `'/api:'` is invalid, and `'/api:/-'` means the literal destination `/-`.
- Prefixes are literal **escaped URL paths**, not regex or wildcards; the source and any destination other than `-` must be non-empty and start with `/`, not `//`. No scheme, authority, query, fragment, whitespace, extra literal colon, or malformed percent escape is allowed. Use percent escapes for characters that require URL encoding.
- No slash normalization or path cleaning is performed, except that prefix deletion ensures a leading `/` and uses `/` for an empty result (so `/api/:-` maps `/api/models` to `/models`). `/api:/v1` maps `/api/` to `/v1/`. Trailing slashes in the rules are literal: `/api/:/v1/` also maps `/api/models` to `/v1/models`, while `/api:/v1/` produces `/v1//models`. For a root prefix, use `/:/v1/` to map `/models` to `/v1/models`.
- Encoded suffixes are preserved: `/api/a%2Fb%20c` becomes `/v1/a%2Fb%20c`. Encoded separators are not boundaries: `/api%2Fmodels` does not match `/api`; `/%61pi` also does not match `/api`.
- The query string stays unchanged, including parameter order, repeated values, encoding, and an empty `?`. No matching rule means the original path is retained. `base_url` remains a bare origin — do not put `/v1` there.
- Rules also work with `target.peer`; each gateway in a chain independently applies its selected target's rules once. They do not change model selection, authentication, body content, or responses.

### Model routing

Each key entry may declare the models it serves:

```yaml
keys:
  - token: sk-virt-dev
    target:
      base_url: https://api.example.com
      api_key: sk-real-replace-me
    models:
      - id: mimo-v2.6-flash
        name: mimo-v2.6-flash
      - id: mimo-v2.6-pro
        name: mimo-v2.6-pro
```

- `id` is the model name callers put in their requests, `name` is the model name the gateway passes to the provider. When they differ, the gateway rewrites the top-level `model` value of the JSON request body before forwarding; every other body byte stays identical. When they are equal, the body is forwarded unchanged.
- For a request whose JSON body carries a top-level `model` **string**, the pair (virtual key, model id) picks the entry: the request goes to that entry's target and the token's round-robin counter is not touched. So several entries under one token can route different models to different providers.
- A `model` value that no entry under the token declares returns `400 Bad Request`; no provider is contacted.
- A caller id ending in `[1m]` (Claude Code's 1M-context marker) falls back to the id without the suffix when that exact id is not declared itself: the request routes to the same entry and the suffix is appended to the entry's `name` before forwarding. With `id: claude-haiku-5-5` and `name: deepseek-v4.1-flash`, the caller's `claude-haiku-5-5[1m]` reaches the provider as `deepseek-v4.1-flash[1m]`. An explicitly declared `[1m]` id wins over this fallback, and the suffix match is case-sensitive.
- Requests without a resolvable `model` — no body, `GET /v1/models`, a non-JSON body, a `model` that is not a string, or a compressed body — keep their bodies byte-for-byte unchanged and pick their target by round-robin as before; any selected target's path rewrite rules still apply. A missing `model` is then the provider's complaint to relay, not the gateway's.
- A token whose entries declare no `models` still forwards without model validation or rewriting. The gateway inspects the body only to log its top-level `model` string; when available, `model.id` and `model.name` both contain the caller's model name.
- `id` values must be unique within one token (a duplicate is a startup error); the same `id` under different tokens is fine.
- Body inspection buffers up to 32 MiB (plus one byte to detect overflow). On model-enabled tokens, larger bodies are rejected with `413 Request Entity Too Large`. On tokens without models, larger bodies pass through without model logging or rejection. Bodies with a non-identity `Content-Encoding` always pass through uninspected and unmodified.

### Encrypted gateway-to-gateway links (e2ee)

Gateways can be chained — gateway B forwards to gateway C, which forwards to the provider. Without protection, that hop carries the URL, request body and the real provider key in plaintext across whatever network sits between the machines. The optional `e2ee` section encrypts **exactly that hop**: the entire TCP byte stream between two gateways is sealed with the Noise IK protocol (X25519 identity keys, per-connection forward-secret session keys). The client→gateway and gateway→provider links are unchanged (plain HTTP / standard HTTPS); only gateway↔gateway traffic is encrypted. HTTP semantics are untouched, so SSE streaming still arrives chunk by chunk through the encrypted link.

```
client ──plain HTTP──► gateway B ══Noise IK encrypted══► gateway C ──plain HTTPS──► provider
                      (virtual key)   URL+body+auth密文     (swaps to real key)
```

There are no client/server roles: every gateway runs the same shape (its own listener, its own identity, a list of pinned peers) and either side can initiate encrypted calls to the other.

**Setup** — on each gateway, generate an identity and exchange public keys out of band (a chat message, not the network):

```sh
llm-gateway --keygen
# identity_key: <base64, stays on THIS machine, never share>
# identity_pub:  <base64, give this to the peer operator>
```

Gateway B's config (calling C):

```yaml
listen: "0.0.0.0:8080"       # clients connect here, plain HTTP
e2ee:
  listen: "0.0.0.0:8081"     # peers connect here, Noise-encrypted
  identity_key: "<B's private key from --keygen>"
  peers:
    - name: gateway-c
      base_url: "http://C.example.com:8081"   # C's e2ee.listen address
      identity_pub: "<C's public key>"
keys:
  - token: sk-virt-from-client
    target:
      peer: gateway-c        # forward over the encrypted link
      api_key: sk-virt-on-c  # the virtual key C accepts
```

Gateway C's config (receiving from B, forwarding to the provider):

```yaml
listen: "0.0.0.0:8080"
e2ee:
  listen: "0.0.0.0:8081"
  identity_key: "<C's private key>"
  peers:
    - name: gateway-b
      base_url: "http://B.example.com:8081"
      identity_pub: "<B's public key>"
keys:
  - token: sk-virt-on-c          # matches B's target.api_key
    target:
      base_url: https://api.openai.com
      api_key: sk-real-provider
```

Rules and guarantees:

- `target.peer` and `target.base_url` are mutually exclusive; `peer` must name a declared `e2ee.peers` entry, and startup validation rejects a peer address that collides with any plaintext `base_url`.
- **Identity pinning is the trust anchor.** Each side pins the other's public key in config; a handshake with any other key (including an active man-in-the-middle on the public internet) fails immediately and no application data is exchanged. Exchange public keys out of band, exactly like exchanging the `api_key` values.
- Every TCP connection performs one handshake; requests on that connection (HTTP keep-alive) reuse the session. Sessions are forward-secret: compromising a long-term identity key does not decrypt recorded past traffic.
- **Firewall the e2ee port.** The handshake rejects unknown identities, but the port should still only accept TCP from known peer addresses (security groups / `ufw` / firewall rules). It binds to loopback by default; set it explicitly for cross-machine links.
- **Key rotation:** generate a new `--keygen` pair, update the peer's `identity_pub` on the other side and this machine's `identity_key`, then restart both gateways. There is no key distribution or revocation mechanism — config is the source of truth.
- Private keys never appear in logs; a rejected handshake logs the remote address and the failure only.

## Running

Linux / macOS:

```sh
./bin/llm-gateway --config ./config.yaml
```

Windows PowerShell:

```powershell
.\bin\llm-gateway.exe --config .\config.yaml
```

Both `--config` and `-config` work; when omitted, `~/.config/llm-gateway/config.yaml` is read (the home directory's `.config` folder, e.g. `C:\Users\<you>\.config\llm-gateway\config.yaml` on Windows). With GNU Make you can also run `make run CONFIG=./config.yaml`.

Press Ctrl+C to shut down; SIGTERM is also supported on POSIX systems. After a shutdown signal, in-flight requests get up to 10 seconds to finish before connections are closed.

### Background mode

`--daemon` starts the gateway detached from the terminal and returns immediately:

```sh
./bin/llm-gateway --daemon --config ./config.yaml
```

- The binary re-launches itself as a detached process. JSON logs are appended to `~/.config/llm-gateway/llm-gateway.log` (override with `--log-file`), and the process id is recorded in `~/.config/llm-gateway/llm-gateway.pid` (override with `--pid-file`). The command returns only after the gateway is actually listening; on failure it reports the log file to inspect.
- `--stop` shuts the gateway down and removes the pid file. On POSIX systems it receives SIGTERM and gets the same 10-second drain as Ctrl+C before being killed; on Windows it is terminated immediately and in-flight requests are cut.
- Only one gateway can hold a pid file: starting a second one reports `already running`, and `--stop` with no running gateway reports `not running` and exits non-zero.
- `--log-file` and `--pid-file` also work in the foreground. A foreground instance only records a pid file when `--pid-file` is given explicitly, and then `--stop` can shut it down too.
- This is process detachment, not a system service: nothing starts the gateway on boot or restarts it after a crash — use systemd, launchd or the Windows service script described below. Like any pid-file scheme, `--stop` trusts the recorded pid; if the operating system has recycled that pid for an unrelated process, that process is targeted.

### Windows service

The binary has no native Windows service (SCM) integration, so `scripts\service-win.bat` wraps it with [NSSM](https://nssm.cc). It needs an Administrator prompt (it requests elevation itself); `nssm.exe` is picked up from `bin\`, then from `PATH`, and otherwise downloaded automatically:

```bat
scripts\service-win.bat install                :: uses .\config.yaml if present
scripts\service-win.bat install C:\path\to\config.yaml
scripts\service-win.bat status
scripts\service-win.bat stop
scripts\service-win.bat uninstall
```

- `install` registers `llm-gateway` as an auto-start service and starts it immediately. The config path is optional: an explicit argument wins, then the repo's `config.yaml`, then `~/.config/llm-gateway/config.yaml`; with none of those the gateway falls back to the LocalSystem home directory and the script warns.
- The gateway runs in the foreground under NSSM, which captures its JSON stdout into rotating files under `%ProgramData%\llm-gateway\` (10 MB per file) and restarts it if it exits. `stop` sends the usual shutdown signal and allows the 10-second request drain to finish.

### Docker

Multi-arch images (`linux/amd64`, `linux/arm64`) are published to [`kunxu984/coniferous-cmd`](https://hub.docker.com/r/kunxu984/coniferous-cmd) on Docker Hub whenever a `v*` tag is pushed; other pushes only build the image to validate the Dockerfile. Mount a config that listens on all interfaces:

```sh
docker run --rm -p 8080:8080 \
  -v "$PWD/config.yaml:/etc/llm-gateway/config.yaml:ro" \
  kunxu984/coniferous-cmd:latest
```

The image runs as a non-root user and reads `--config /etc/llm-gateway/config.yaml` by default, so `listen` must be `0.0.0.0:8080` for the port mapping to reach the gateway.

## Request examples

With Ollama running locally and the model `llama3.2` installed, using the sample `sk-virt-local`:

```sh
curl http://127.0.0.1:8080/v1/chat/completions -H 'Authorization: Bearer sk-virt-local' -H 'Content-Type: application/json' --data-binary '{"model":"llama3.2","messages":[{"role":"user","content":"Hello"}],"stream":false}'
```

For SSE streaming requests use `--no-buffer`:

```sh
curl --no-buffer http://127.0.0.1:8080/v1/chat/completions -H 'Authorization: Bearer sk-virt-local' -H 'Content-Type: application/json' --data-binary '{"model":"llama3.2","messages":[{"role":"user","content":"Hello"}],"stream":true}'
```

The curl commands above use POSIX shell quoting. On Windows they run as-is in Git Bash; in PowerShell use `curl.exe` and quote the JSON argument according to your PowerShell version.

The gateway does not require a `/v1` prefix, nor does it restrict methods or content types. Once authenticated, any path is forwarded to a configured upstream — this is not a general-purpose proxy for arbitrary client-chosen addresses.

### Forwarding boundaries and errors

- The connection address and HTTP `Host` are updated to the selected target (including non-default ports), so the gateway's local address is never sent to the upstream's virtual host. Beyond that, authentication is rewritten, the path may be rewritten by the selected target's `rewrite` rules, and on model-enabled keys the body's top-level `model` value may be rewritten when an alias is configured. Without a matching rule the original path (including encoding) stays unchanged; the query and remaining application-layer headers always stay unchanged. No forwarding headers such as `X-Forwarded-For` are added or appended; existing values are preserved.
- Hop-by-hop headers are handled per standard HTTP proxy rules — e.g. `Connection`, `Keep-Alive`, and any headers it names; the HTTP transport may re-frame the stream, so this is not a byte-level TCP tunnel.
- Status codes, upstream application-layer response headers, and the body are passed through unchanged; no automatic decompression, SSE is forwarded chunk by chunk, and there is no overall write timeout. Connect timeout is 10 s, response-header timeout 30 s.
- Missing, malformed, or unknown virtual key: `401 Unauthorized`, and no upstream is contacted.
- Unknown model id on a model-enabled key: `400 Bad Request`; a request body beyond the inspection limit: `413 Request Entity Too Large`. In both cases no upstream is contacted.
- Upstream connection failure: `502 Bad Gateway`; no switch to another target. The next request continues the round-robin sequence.
- Upstream HTTP errors such as `429`: status code and body are passed through as-is.
- Operational JSON logs go to stdout, or to the `--log-file` destination, one line per request, with method, path (query excluded), target, status, and duration. They do not include auth headers or bodies. When conversation capture is enabled, authenticated calls also include a `request_id` linking to their separate conversation file.

## Conversation files (opt-in)

To keep the request and reply for each individual API call, add this top-level section to your YAML configuration and restart:

```yaml
conversation_log:
  enabled: true
  directory: logs/conversations
  max_body_bytes: 8388608
```

Logging is **disabled by default**. Relative directories (including the default `logs/conversations`) resolve beside the configuration file, not against the working directory; this also applies with `--daemon`. Absolute paths are supported. `max_body_bytes` must be a positive integer and defaults to **8 MiB for each body independently**. Configuration is read once at startup.

Each authenticated call, including local validation failures and upstream errors, produces a separate file when its handler finishes:

```text
logs/conversations/2026-10-26/143025_3b3eb8f7a2a14119a4bdf2bb792e8c2d.json
```

The date and time use UTC at request start. IDs are generated by the gateway; clients need not send a session ID. Calls in one multi-turn conversation are **not merged**. Unauthorized requests have operational summaries only, with no captured bodies. Ordinary and decrypted encrypted-listener requests use the same capture behavior; chained gateways log their own hops independently.

A representative version 1 file for a plain-text request/reply is:

```json
{
  "schema_version": 1,
  "request_id": "3b3eb8f7a2a14119a4bdf2bb792e8c2d",
  "started_at": "2026-10-26T14:30:25Z",
  "ended_at": "2026-10-26T14:30:25.15Z",
  "duration_ms": 150,
  "method": "POST",
  "path": "/v1/chat/completions",
  "upstream": "https://api.example.com",
  "status": 200,
  "response_complete": true,
  "request": {
    "data": "Hello",
    "encoding": "utf-8",
    "observed_bytes": 5,
    "captured_bytes": 5,
    "truncated": false,
    "complete": true
  },
  "response": {
    "data": "Hi",
    "encoding": "utf-8",
    "observed_bytes": 2,
    "captured_bytes": 2,
    "truncated": false,
    "complete": true
  }
}
```

Matched models add `model_id` and `model_name`. Request capture preserves the original client body before model alias rewriting, while the response contains bytes successfully written through the gateway's response writer (not a guarantee the remote client received them). JSON bodies remain text inside `data`; SSE replies retain raw event lines rather than reconstructed assistant messages. Compressed/binary or invalid UTF-8 prefixes use `encoding: "base64"` for lossless storage; no decompression is performed and HTTP headers are not saved.

`truncated` means the retained prefix hit the capture limit and further bytes were observed. It does not stop forwarding or cause a rejection. `complete` describes transfer observation, independently of truncation: an interrupted reply can have HTTP status 200 but `response_complete: false` and a safe error category. Unread or partially read requests are marked incomplete and are not drained just for logging; `observed_bytes` is not necessarily the full original body size. Complete upstream HTTP errors such as 429 still have `response_complete: true`. Calls aborted before committing a final status may omit `status`.

Streaming chunks are forwarded immediately; bounded capture stays in memory, and a complete JSON record is published after forwarding ends. `--log-file` still controls only operational logs. Storage initialization or write errors produce safe operational diagnostics and do not fail calls or stop startup. Storage that is arbitrarily slow can delay handler cleanup; this is not an asynchronous audit service.

**Security and retention:**

- Request/response headers, raw query strings, virtual keys, upstream keys, and encryption private keys are not copied into structured metadata. **Bodies are unredacted** and may contain any of those secrets themselves, code, personal information, or tool output. Paths and model names can also contain user-provided information. Enable capture only where storing this content is acceptable.
- Files are plaintext at rest, including content received over the encrypted peer channel. Use a protected disk/volume if encryption at rest is required.
- Newly created directories request mode 0700 and files 0600 where supported. Existing parent-directory permissions and symlinks are not rewritten. On Windows, explicitly restrict directory ACLs to the gateway account and trusted administrators; Unix mode bits alone do not secure Windows files.
- There is **no automatic retention/deletion**. Provision disk space and use your OS scheduler or other operator-managed process to prune old date directories. Per-body limits bound individual records, not total disk use or aggregate memory across concurrent requests.
- Forced termination, crashes, or shutdown timeout can lose active-call records and leave temporary files. Final `.json` files are published only after serialization and close; this is not a crash-recovery journal. Filesystems must support same-directory hard links for non-overwriting publication; unsupported storage reports persistence failures rather than overwriting records.

## Python request test scripts

The two scripts use the official `openai` and `anthropic` Python SDKs, print the HTTP status, response headers, and body, and exit with code 1 on non-2xx. SDK auto-retries are disabled, and system proxy settings are ignored so the scripts connect directly to the gateway. Dependencies are installed into the development `agents` environment and do not affect the Go gateway's runtime dependencies:

```sh
mamba run -n agents python -m pip install openai anthropic
```

The gateway must already be running. The default virtual key is `sk-virt-dev`, or supply one via the `GATEWAY_TOKEN` environment variable; never give the scripts a real upstream key.

```sh
mamba run -n agents python scripts/test_openai.py
mamba run -n agents python scripts/test_anthropic.py
```

The default model is `mimo-v2.6-flash`, which has been verified working; override it with `--model`. In the Xiaomi endpoint test, model names with the `[1m]` suffix returned `400 Unsupported model`. The OpenAI default base URL is `http://127.0.0.1:8080/v1` (SDK configs include `/v1`), the Anthropic one is `http://127.0.0.1:8080/anthropic`; both can be changed with `--base-url`.

Both scripts use the Bearer authentication the gateway supports. `test_anthropic.py --auth-mode api-key` can exercise the x-api-key-only mode, but the current gateway does not accept that auth scheme, so it cannot be used to judge an upstream's authentication capabilities.

## Development and cross-platform verification

```sh
go test ./...
go vet ./...
go test -race ./...
```

Regular tests need no C compiler. **Race detection is a development-time requirement**: it needs `CGO_ENABLED=1` on a supported platform with a compatible native C toolchain; release builds may still use `CGO_ENABLED=0`. If MinGW-w64 / GCC or a comparable toolchain is missing on Windows, run the regular tests — but do not treat that as completing race verification.

`cmd/llm-gateway/main_test.go` compiles the real program, creates a temp config and two mock upstreams, and verifies `--config`, 401, round-robin, non-`/v1` paths, and SSE via `curl`. This test needs curl on PATH; it skips when curl is missing. Run it alone with:

```sh
go test -count=1 -v ./cmd/llm-gateway
```

With GNU Make:

```sh
make help
make daemon
make stop
make check
make test-race
make build-all
make build-linux-amd64
make build-windows-arm64
make build-darwin-arm64
```

`make build-all` produces Windows, Linux, and macOS executables for amd64 / arm64 under `bin/`. A successful cross-compile does not prove the binary runs on that OS; CI runs native tests on all three OS families and runs race detection on Linux.

Cross-compiling without Make also works, e.g. in a POSIX shell:

```sh
CGO_ENABLED=0 GOOS=linux GOARCH=amd64 go build -trimpath -o bin/llm-gateway-linux-amd64 ./cmd/llm-gateway
```

Confirm the CGO-free Linux build is `statically linked` with `file bin/llm-gateway-linux-amd64`. Windows and macOS builds still use the OS-provided interfaces and need no extra language runtime installed.
