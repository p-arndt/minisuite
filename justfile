# minicloak — task runner
#
# Install `just`:  winget install Casey.Just   (or  cargo install just)
# List recipes:    just            (or  just --list)
#
# minicloak is a single-crate, dependency-free Rust project. The `smoke` recipe
# starts a release build in the background and runs the Python end-to-end suite
# against it. Recipes are POSIX shell so `smoke` works from Git Bash on Windows
# as well as Linux/macOS.
set shell := ["bash", "-cu"]
set windows-shell := ["bash", "-cu"]

# Default: show the recipe list.
default:
    @just --list

# ---------------------------------------------------------------------------
# Dev
# ---------------------------------------------------------------------------

# Run the server from source, passing through any args:  just run --auto-login alice
run *ARGS:
    cargo run -- {{ARGS}}

# Build a debug binary.
build:
    cargo build

# ---------------------------------------------------------------------------
# Quality
# ---------------------------------------------------------------------------

# Run the Rust test suite.
test:
    cargo test

# Format all Rust code.
fmt:
    cargo fmt --all

# Lint with clippy (warnings as errors).
clippy:
    cargo clippy --all-targets -- -D warnings

# Build a release binary, start it on :19500 in the background, run the Python
# end-to-end suite against it, then shut it down. Works from Git Bash on Windows.
smoke:
    cargo build --release
    ./target/release/minicloak --bind 127.0.0.1:19500 --key target/smoke-key.pem & \
    server=$!; \
    trap "kill $server 2>/dev/null" EXIT; \
    sleep 1; \
    python smoketest.py http://127.0.0.1:19500

# ---------------------------------------------------------------------------
# Docker
# ---------------------------------------------------------------------------

# Build the scratch image locally.
docker:
    docker build -t minicloak:dev .
