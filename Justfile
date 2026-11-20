# Justfile - Convenient command runner for ReiClaw development
# https://github.com/casey/just

# Default recipe to display help
_default:
    @just --list

# Format all code
fmt: toolchain
    cargo fmt --all

# Check formatting without making changes
fmt-check: toolchain
    cargo fmt --all -- --check

# Run clippy lints
lint: toolchain
    cargo clippy --all-targets -- -D warnings

# Run all tests
test: toolchain
    cargo test --locked

# Run only unit tests (faster)
test-lib: toolchain
    cargo test --lib

# Verify the active cargo/rustc match the rust-toolchain.toml pin
toolchain:
    ./scripts/ci/toolchain_gate.sh

# Run the full CI quality gate locally
ci: toolchain fmt-check lint test
    @echo "✅ All CI checks passed!"

# Build in release mode
build: toolchain
    cargo build --release --locked

# Build in debug mode
build-debug: toolchain
    cargo build

# Build slim personal-agent profile
build-slim: toolchain
    cargo build --locked -p reiclaw --bin rei --no-default-features --features agent-runtime,gateway

# Check slim personal-agent profile
check-slim: toolchain
    cargo check --locked -p reiclaw --all-targets --no-default-features --features agent-runtime,gateway

# Test slim personal-agent profile
test-slim: toolchain
    cargo test --locked -p reiclaw --bin rei --no-default-features --features agent-runtime,gateway

# Clean build artifacts
clean: toolchain
    cargo clean

# Run rei with example config (for development)
dev *ARGS: toolchain
    cargo run -- {{ARGS}}

# Check code without building
check: toolchain
    cargo check --all-targets

# Run cargo doc and open in browser
doc: toolchain
    cargo doc --no-deps --open

# Update dependencies
update: toolchain
    cargo update

# Run cargo audit to check for security vulnerabilities
audit: toolchain
    cargo audit

# Run cargo deny checks
deny: toolchain
    cargo deny check

# Format TOML files (requires taplo)
fmt-toml:
    taplo format

# Check TOML formatting (requires taplo)
fmt-toml-check:
    taplo format --check

# Run all formatting tools
fmt-all: fmt fmt-toml
    @echo "✅ All formatting complete!"
