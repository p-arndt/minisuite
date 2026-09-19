# rust.just — Cargo recipes. Synced from github.com/p-arndt/just-common.
#
# May override (needs `set allow-duplicate-variables`):
#   CARGO_SCOPE := "-p mycrate"     default "--workspace"
#   RUN_PKG     := "mycrate"        crate `just run` starts, default: cargo's default

CARGO_SCOPE := "--workspace"
RUN_PKG := ""

# Run from source, passing args through:  just run --help
run *ARGS:
    cargo run {{ if RUN_PKG == "" { "" } else { "-p " + RUN_PKG } }} -- {{ARGS}}

# Build (debug).
build:
    cargo build {{CARGO_SCOPE}}

# Build optimized.
build-release:
    cargo build {{CARGO_SCOPE}} --release

# Type-check without building (faster).
check:
    cargo check {{CARGO_SCOPE}} --all-targets

# Run the test suite.
test:
    cargo test {{CARGO_SCOPE}}

# Format all Rust code.
fmt:
    cargo fmt --all

# Fail if anything is unformatted.
fmt-check:
    cargo fmt --all --check

# Lint with clippy, warnings as errors.
clippy:
    cargo clippy {{CARGO_SCOPE}} --all-targets -- -D warnings

alias lint := clippy

# Every check CI runs. Override with `set allow-duplicate-recipes` to add e.g. smoke tests.
ci: fmt-check clippy test

# Remove build artifacts.
clean:
    cargo clean
