# syntax=docker/dockerfile:1

# --- Build stage ---
FROM golang:1.26-alpine AS build
WORKDIR /src

# Cache module downloads separately from source changes.
COPY go.mod go.sum ./
RUN go mod download

COPY cmd ./cmd
COPY internal ./internal

# Pure Go release build, same flags as `make build` plus stripped symbols.
RUN CGO_ENABLED=0 go build -trimpath -ldflags="-s -w" \
    -o /out/llm-gateway ./cmd/llm-gateway

# --- Runtime stage ---
FROM alpine:3.22
# ca-certificates are required for HTTPS calls to upstream providers.
RUN apk add --no-cache ca-certificates \
    && adduser -D -H -u 65532 gateway

COPY --from=build /out/llm-gateway /usr/local/bin/llm-gateway

USER gateway
ENV HOME=/home/gateway

# Mount your config here; it must set `listen: 0.0.0.0:8080` to accept
# connections from outside the container.
VOLUME /etc/llm-gateway
EXPOSE 8080

ENTRYPOINT ["/usr/local/bin/llm-gateway"]
CMD ["--config", "/etc/llm-gateway/config.yaml"]
