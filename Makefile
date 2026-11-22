.PHONY: all build build-release install install-release clean test ci check fmt lint run demo release bench bench-validate

LEVEL ?= minor

# Default target
all: check build test

# Build debug version
build:
	cargo build

# Build release version
build-release:
	cargo build --release

# Install debug binary to ~/.cargo/bin
install:
	CARGO_INCREMENTAL=0 cargo install --path . --locked --bins --debug --force

# Install release binary to ~/.cargo/bin
install-release:
	CARGO_INCREMENTAL=0 cargo install --path . --locked --bins --force

# Clean build artifacts
clean:
	cargo clean

# Run tests (requires cargo-nextest)
test:
	cargo nextest run --no-tests=pass

# Mirror the CI workflow: fmt check, clippy, tests
ci: lint test

# Run clippy and check
check:
	cargo check
	cargo clippy -- -D warnings

# Format code
fmt:
	cargo fmt

# Lint (check formatting)
lint:
	cargo fmt -- --check
	cargo clippy -- -D warnings

# Run with arguments (usage: make run ARGS="status")
run:
	cargo run -- $(ARGS)

# Quick demo
demo: install
	@echo "=== mem-in-black demo ==="
	mib --help

# Task benchmark. Calls a real model and spends your tokens (see bench/README.md).
# This target never passes --yes: it prints the session count and cost ceiling and
# the command to run. Start a paid run with:
#   make bench BENCH_ARGS="--task handoff-currency --runs 1 --model haiku --yes"
bench: build
	node bench/run.mjs $(BENCH_ARGS)

# Check every benchmark task without calling a model (free)
bench-validate: build
	node bench/run.mjs --validate $(BENCH_ARGS)

# Bump version, regenerate CHANGELOG.md, tag, and push (requires cargo-release + git-cliff)
release:
	cargo release $(LEVEL) --execute --no-confirm
