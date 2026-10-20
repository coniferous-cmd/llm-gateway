# GNU Make. Requires Go; no Unix-specific commands are used in recipes.
# Override tools/paths: make build GO=/path/to/go CONFIG=./config.yaml
.DEFAULT_GOAL := help

GO ?= go
CONFIG ?= config.yaml
BIN_DIR ?= bin
HOST_OS := $(shell $(GO) env GOOS)
EXE := $(if $(filter windows,$(HOST_OS)),.exe,)
BINARY := $(BIN_DIR)/llm-gateway$(EXE)

# Release builds are pure Go and need no C compiler or shared Go runtime.
export CGO_ENABLED := 0

.PHONY: help build run test test-race vet fmt check build-all

help:
	@echo make build       - Build the native executable in $(BIN_DIR)
	@echo make run         - Build and run with CONFIG=config.yaml
	@echo make test        - Run unit tests
	@echo make test-race   - Run race tests; requires a native C compiler
	@echo make vet         - Run go vet
	@echo make fmt         - Format Go source
	@echo make check       - Run tests and vet
	@echo make build-all   - Build Windows, Linux and macOS for amd64 and arm64
	@echo Override config: make run CONFIG=./config.example.yaml

build:
	$(GO) build -trimpath -o "$(BINARY)" ./cmd/llm-gateway

run: build
	"./$(BINARY)" --config "$(CONFIG)"

test:
	$(GO) test ./...

# Race instrumentation requires CGO only at test/build time, not in releases.
test-race: export CGO_ENABLED := 1
test-race:
	$(GO) test -race ./...

vet:
	$(GO) vet ./...

fmt:
	$(GO) fmt ./...

check: test vet

# Target-specific environment variables work with both Windows and POSIX shells.
# go build creates the output directory, so mkdir is not needed.
define platform
.PHONY: build-$(1)-$(2)
build-all: build-$(1)-$(2)
build-$(1)-$(2): export GOOS := $(1)
build-$(1)-$(2): export GOARCH := $(2)
build-$(1)-$(2):
	$$(GO) build -trimpath -o "$$(BIN_DIR)/llm-gateway-$(1)-$(2)$(3)" ./cmd/llm-gateway
endef

$(eval $(call platform,windows,amd64,.exe))
$(eval $(call platform,windows,arm64,.exe))
$(eval $(call platform,linux,amd64,))
$(eval $(call platform,linux,arm64,))
$(eval $(call platform,darwin,amd64,))
$(eval $(call platform,darwin,arm64,))
