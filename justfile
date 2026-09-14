# minisuite — workspace task runner
#
# Install `just`:  winget install Casey.Just   (or  cargo install just)
# List recipes:    just            (or  just --list)
#
# The release recipes need `stamp` (https://github.com/p-arndt/stamp) on PATH:
#   Windows:        irm https://raw.githubusercontent.com/p-arndt/stamp/main/install.ps1 | iex
#   macOS / Linux:  curl -fsSL https://raw.githubusercontent.com/p-arndt/stamp/main/install.sh | sh
#
# minisuite is a Cargo workspace with four std-only binaries — minicloak (OIDC),
# minimail (SMTP sink), minibucket (S3) and minisuite (all three in one process).
# Recipe bodies stay one line each and shell out to scripts/*.mjs for anything
# involving background processes, so every recipe behaves identically under
# PowerShell on Windows and sh on Linux/macOS.

# Run recipes through PowerShell on Windows (the default is cmd.exe).
set windows-shell := ["pwsh.exe", "-NoLogo", "-NoProfile", "-Command"]

alias lint := clippy

# Default: show the recipe list.
default:
    @just --list

# ---------------------------------------------------------------------------
# Dev
# ---------------------------------------------------------------------------

# Run a binary from source:  just run                 (the whole suite)
#                            just run minicloak --auto-login alice
run crate="minisuite" *ARGS:
    cargo run -p {{crate}} -- {{ARGS}}

# Build every crate (debug).
build:
    cargo build --workspace

# Build every crate optimized (-> target/release/{minicloak,minimail,minibucket,minisuite}).
build-release:
    cargo build --workspace --release

# ---------------------------------------------------------------------------
# Quality
# ---------------------------------------------------------------------------

# Type-check the workspace (faster than a full build).
check:
    cargo check --workspace --all-targets

# Run the Rust test suite.
test:
    cargo test --workspace

# Format all Rust code.
fmt:
    cargo fmt --all

# Verify formatting without writing changes (CI gate).
fmt-check:
    cargo fmt --all --check

# Lint with clippy (warnings as errors).
clippy:
    cargo clippy --workspace --all-targets -- -D warnings

# End-to-end Python smoke tests: build, start the server, run smoketest.py, stop.
#   just smoke              # all three, sequentially
#   just smoke minibucket   # one service  (needs: pip install boto3)
smoke crate="all":
    node scripts/smoke.mjs {{crate}}

# The full local CI gate — mirrors .github/workflows/ci.yml. Run before pushing.
ci: fmt-check clippy test smoke

# ---------------------------------------------------------------------------
# Docker (mirrors what the release workflow publishes to ghcr.io)
# ---------------------------------------------------------------------------

# Build one scratch image from the multi-target root Dockerfile.
#   just docker             # the all-in-one suite
#   just docker minibucket
docker target="minisuite":
    docker build --target {{target}} -t {{target}}:dev .

# Build all four images.
docker-all: (docker "minicloak") (docker "minimail") (docker "minibucket") (docker "minisuite")

# The docker-run-* recipes publish ports on 127.0.0.1 only: the images ship
# with trivial dev credentials. To reach a service from other hosts drop the
# `127.0.0.1:` prefix (`-p 9500:9500`) and set your own credentials via -e.

# Run the all-in-one image: OIDC 9500, S3 9000, SMTP 1025, mail UI 8025, landing 9900.
docker-run-minisuite:
    docker run --rm -p 127.0.0.1:9500:9500 -p 127.0.0.1:9000:9000 -p 127.0.0.1:1025:1025 -p 127.0.0.1:8025:8025 -p 127.0.0.1:9900:9900 -v minisuite-data:/data minisuite:dev

# Run just the OIDC provider.
docker-run-minicloak:
    docker run --rm -p 127.0.0.1:9500:9500 -v minicloak-data:/data minicloak:dev

# Run just the SMTP sink + web UI.
docker-run-minimail:
    docker run --rm -p 127.0.0.1:1025:1025 -p 127.0.0.1:8025:8025 -v minimail-data:/data minimail:dev

# Run just the S3 server.
docker-run-minibucket:
    docker run --rm -p 127.0.0.1:9000:9000 -v minibucket-data:/data minibucket:dev

# Run a locally built image with the right port mapping:  just docker-run minimail
docker-run target="minisuite":
    just docker-run-{{target}}

# Start the published stack in the background (see compose.yml).
compose-up:
    docker compose up -d

# Stop the stack (named volumes survive; add -v by hand to wipe them).
compose-down:
    docker compose down

# ---------------------------------------------------------------------------
# Release
# ---------------------------------------------------------------------------

# Print the one version shared by every crate and image (see .stamp.yml).
version:
    @stamp current

# Write a version into Cargo.toml + Cargo.lock WITHOUT committing. Accepts a
# bump keyword or an explicit version. This is the correction command; to cut a
# release use `just release`.
#   just set-version patch        just set-version 0.3.0
set-version BUMP="patch":
    stamp set {{BUMP}} && cargo update --workspace

# Cut a release: bump the version, refresh Cargo.lock, commit, tag `v<x.y.z>`
# and push -> triggers the release workflow (multi-arch archives + four ghcr.io
# images). Refuses to run on a dirty tree.
#   just release            just release minor            just release 1.0.0
release BUMP="patch":
    node scripts/release.mjs {{BUMP}}

# Show what `just release BUMP` would do — the plan and every stamp check —
# without writing anything.
release-dry BUMP="patch":
    node scripts/release.mjs {{BUMP}} --dry-run

# ---------------------------------------------------------------------------
# Housekeeping
# ---------------------------------------------------------------------------

# Remove build artifacts (including target/smoke).
clean:
    cargo clean
