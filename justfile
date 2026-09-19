# minisuite — workspace task runner
#
# Install `just`:  winget install Casey.Just   (or  cargo install just)
# List recipes:    just
#
# The release recipes need `stamp` (https://github.com/p-arndt/stamp) on PATH:
#   Windows:        irm https://raw.githubusercontent.com/p-arndt/stamp/main/install.ps1 | iex
#   macOS / Linux:  curl -fsSL https://raw.githubusercontent.com/p-arndt/stamp/main/install.sh | sh
#
# Shared recipes (build, test, fmt, clippy/lint, version, …) live in .just/, copied
# from ~/coding/just-common. Edit them there and run `just sync-common`; this file
# only holds what is specific to minisuite and the recipes it overrides.

import '.just/common.just'
import '.just/rust.just'
import '.just/release.just'

set allow-duplicate-recipes

# ---------------------------------------------------------------------------
# Overrides of shared recipes
# ---------------------------------------------------------------------------

# Run a binary from source:  just run                 (the whole suite)
#                            just run minicloak --auto-login alice
run crate="minisuite" *ARGS:
    cargo run -p {{crate}} -- {{ARGS}}

# The full local CI gate — mirrors .github/workflows/ci.yml. Run before pushing.
ci: fmt-check clippy test smoke

# ---------------------------------------------------------------------------
# Testing
# ---------------------------------------------------------------------------

# End-to-end Python smoke tests: build, start the server, run smoketest.py, stop.
#   just smoke              # all three, sequentially
#   just smoke minibucket   # one service  (needs: pip install boto3)
smoke crate="all":
    node scripts/smoke.mjs {{crate}}

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
