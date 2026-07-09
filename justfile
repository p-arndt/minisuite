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

alias lint := clippy

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

# Verify formatting without changing files (CI gate).
fmt-check:
    cargo fmt --all --check

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

# The full local CI gate — mirrors .github/workflows/ci.yml. Run before pushing.
ci: fmt-check clippy test smoke

# ---------------------------------------------------------------------------
# Docker
# ---------------------------------------------------------------------------

# Build the scratch image locally.
docker:
    docker build -t minicloak:dev .

# ---------------------------------------------------------------------------
# Publish
# ---------------------------------------------------------------------------

# Print the current version (from Cargo.toml).
version:
    @node -e "import('./scripts/set-version.mjs').then(m => console.log(m.readVersion()))"

# Stamp a version into Cargo.toml WITHOUT committing. Accepts a bump keyword or
# an explicit version:
#   just set-version patch        just set-version 0.2.0
set-version bump="patch":
    node scripts/set-version.mjs {{bump}}

# Cut a release: bump the version (patch|minor|major, or explicit x.y.z), refresh
# Cargo.lock, commit, tag `v<x.y.z>`, and push -> triggers the "Build and Publish
# Release" workflow (Linux, Windows, macOS binaries + container image). Refuses to
# run on a dirty tree.
#   just release            just release minor            just release 1.0.0
release bump="patch":
    node scripts/release.mjs {{bump}}
