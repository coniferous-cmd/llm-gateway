# GNU Make. Recipes avoid Unix-only commands so they run under POSIX sh,
# bash and Windows shells alike.
# Override tools/paths: make build CARGO=/path/to/cargo CONFIG=./config.yaml
.DEFAULT_GOAL := help

CARGO ?= cargo
# Empty CONFIG lets the binary use its default ~/.config/llm-gateway/config.yaml.
CONFIG ?=
BIN_DIR ?= bin
HOST_OS := $(shell rustc -vV 2>/dev/null | sed -n 's/^host: \(.*-unknown-.*\).*//p')
BINARY := $(BIN_DIR)/llm-gateway
VERSION ?= $(shell git describe --tags --always --dirty 2>/dev/null || echo dev)
export LLM_GATEWAY_VERSION := $(VERSION)

.PHONY: help build run daemon stop test test-release clippy fmt check build-all

# Quotes keep ; ( ) in the help text from being parsed by the recipe shell.
help:
	@echo "make build       - Build the native executable in $(BIN_DIR)"
	@echo "make run         - Build and run; CONFIG=./config.yaml optional"
	@echo "make daemon      - Build and run detached in the background; CONFIG optional"
	@echo "make stop        - Stop the background gateway"
	@echo "make test        - Run all unit and integration tests"
	@echo "make test-release - Run the tests in release mode"
	@echo "make clippy      - Run clippy with warnings denied"
	@echo "make fmt         - Format Rust source"
	@echo "make check       - Run tests, clippy and a fmt check"
	@echo "make build-all   - Cross-build every release target"
	@echo "Override config: make run CONFIG=./config.example.yaml"
	@echo "Override version: make build VERSION=v1.2.3 (default: git describe)"

build:
	$(CARGO) build --release
	@mkdir -p $(BIN_DIR)
	@cp target/release/llm-gateway$(if $(findstring windows,$(HOST_OS)),.exe,) "$(BINARY)"

run: build
	"./$(BINARY)" $(if $(CONFIG),--config "$(CONFIG)")

daemon: build
	"./$(BINARY)" serve --daemon $(if $(CONFIG),--config "$(CONFIG)")

stop:
	"./$(BINARY)" stop

test:
	$(CARGO) test --all-targets

test-release:
	$(CARGO) test --all-targets --release

clippy:
	$(CARGO) clippy --all-targets -- -D warnings

fmt:
	$(CARGO) fmt

check: test clippy
	$(CARGO) fmt --check

# Cross-compiling every release target needs `cross` for the non-native
# ones (Linux musl and Windows); macOS targets build on a macOS runner.
build-all:
	$(CARGO) build --release --target aarch64-apple-darwin
	$(CARGO) build --release --target x86_64-apple-darwin
	@echo "Cross targets (linux musl, windows) are built by CI; see .github/workflows/build.yml"
