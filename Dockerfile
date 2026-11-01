# syntax=docker/dockerfile:1

# --- Build stage ---
FROM rust:1.96-bookworm AS build
ARG TARGETARCH
ARG LLM_GATEWAY_VERSION=dev
ENV LLM_GATEWAY_VERSION=${LLM_GATEWAY_VERSION}
RUN case "$TARGETARCH" in \
        amd64) echo x86_64-unknown-linux-musl > /rust-target ;; \
        arm64) echo aarch64-unknown-linux-musl > /rust-target ;; \
        *) echo "Unsupported architecture: $TARGETARCH" >&2; exit 1 ;; \
    esac
RUN apt-get update \
    && apt-get install -y --no-install-recommends musl-tools \
    && rm -rf /var/lib/apt/lists/* \
    && rustup target add "$(cat /rust-target)"
WORKDIR /src

# Cache dependency downloads separately from source changes.
COPY Cargo.toml Cargo.lock ./
RUN mkdir -p src && echo 'fn main() {}' > src/main.rs \
    && cargo build --release --locked --target "$(cat /rust-target)" || true \
    && rm -rf src

COPY build.rs ./
COPY src ./src
# Static musl build: no shared libc, so the binary runs on any Linux.
RUN target="$(cat /rust-target)" \
    && cargo build --release --locked --target "$target" \
    && install -D -m 0755 "target/$target/release/llm-gateway" /out/llm-gateway

# --- Runtime stage ---
# Distroless/static ships ca-certificates (required for HTTPS to upstream
# providers) and runs as a non-root user.
FROM gcr.io/distroless/static-debian12:nonroot
COPY --from=build /out/llm-gateway /usr/local/bin/llm-gateway

# Mount your config here; it must set `listen: 0.0.0.0:8080` to accept
# connections from outside the container.
VOLUME /etc/llm-gateway
EXPOSE 8080

# The default config/pid/log paths resolve under $HOME, so give the process a
# writable one inside its own layer.
ENV HOME=/home/gateway
USER nonroot:nonroot

ENTRYPOINT ["/usr/local/bin/llm-gateway"]
CMD ["serve", "--config", "/etc/llm-gateway/config.yaml"]
