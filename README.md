# llm-gateway

A lightweight, locally running LLM gateway: callers only store virtual keys; the gateway picks the upstream and swaps `Authorization`. It does not parse API protocols, modify request bodies, count tokens, retry, fail over, or hot-reload configuration.

Supports Windows, Linux, macOS (amd64 / arm64). The release is a single binary with no dependency on Go, Node.js, Python, or a C compiler; the config file is supplied separately. The only third-party Go dependency is `gopkg.in/yaml.v3`, used to parse YAML at startup.

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

- The connection address and HTTP `Host` are updated to the selected target (including non-default ports), so the gateway's local address is never sent to the upstream's virtual host. Beyond that, only authentication is rewritten; the original path (including encoding), query, body, and remaining application-layer headers stay unchanged. No forwarding headers such as `X-Forwarded-For` are added or appended; existing values are preserved.
- Hop-by-hop headers are handled per standard HTTP proxy rules — e.g. `Connection`, `Keep-Alive`, and any headers it names; the HTTP transport may re-frame the stream, so this is not a byte-level TCP tunnel.
- Status codes, upstream application-layer response headers, and the body are passed through unchanged; no automatic decompression, SSE is forwarded chunk by chunk, and there is no overall write timeout. Connect timeout is 10 s, response-header timeout 30 s.
- Missing, malformed, or unknown virtual key: `401 Unauthorized`, and no upstream is contacted.
- Upstream connection failure: `502 Bad Gateway`; no switch to another target. The next request continues the round-robin sequence.
- Upstream HTTP errors such as `429`: status code and body are passed through as-is.
- JSON logs go to stdout, one line per request, with method, path (query excluded), target, status, and duration; auth headers and bodies are never logged.

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
