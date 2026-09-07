# syntax=docker/dockerfile:1

# ---- Build stage --------------------------------------------------------
# Build a fully static musl binary so it can run on an empty scratch image.
#
# Pin the builder to BUILDPLATFORM (the native runner arch) and *cross-compile*
# to the requested target. This avoids running the toolchain under QEMU on a
# mismatched host arch, which makes the linker pick the wrong `cc` (e.g.
# `cc: error: unrecognized command-line option '-m64'`).
FROM --platform=$BUILDPLATFORM rust:1-bookworm AS builder

# Set by BuildKit/buildx to the platform being built (e.g. amd64, arm64).
# Defaults to amd64 for plain `docker build` invocations.
ARG TARGETARCH=amd64

RUN apt-get update \
    && apt-get install -y --no-install-recommends \
       musl-tools \
       gcc-aarch64-linux-gnu \
    && rm -rf /var/lib/apt/lists/* \
    && rustup target add x86_64-unknown-linux-musl aarch64-unknown-linux-musl

# Cross-link the aarch64 musl target with the GNU cross compiler.
ENV CARGO_TARGET_AARCH64_UNKNOWN_LINUX_MUSL_LINKER=aarch64-linux-gnu-gcc

WORKDIR /build

COPY Cargo.toml Cargo.lock ./
# Brings src/ui/ (the embedded web UI assets) along for include_str!.
COPY src ./src

# Map the Docker arch to the matching Rust musl target. Plain `cargo build`
# with no --features keeps the optional `tls` feature (rustls) off.
RUN case "$TARGETARCH" in \
        amd64) RUST_TARGET=x86_64-unknown-linux-musl ;; \
        arm64) RUST_TARGET=aarch64-unknown-linux-musl ;; \
        *)     echo "unsupported TARGETARCH: $TARGETARCH" >&2; exit 1 ;; \
    esac \
    && cargo build --release --target "$RUST_TARGET" \
    && cp "target/$RUST_TARGET/release/minimail" /minimail

# Pre-create the data directory so it lands in the runtime image owned by the
# nonroot user (scratch has no shell to mkdir at runtime).
RUN mkdir -p /data

# ---- Runtime stage ------------------------------------------------------
# A fully static musl binary needs nothing at runtime (no libc, no certs for an
# inbound-only server), so we can ship it on an empty scratch image.
FROM scratch

ARG VERSION=dev
LABEL org.opencontainers.image.title="minimail" \
      org.opencontainers.image.description="A tiny dev SMTP sink with a web UI + JSON API" \
      org.opencontainers.image.source="https://github.com/p-arndt/minimail" \
      org.opencontainers.image.version="$VERSION" \
      org.opencontainers.image.licenses="MIT"

# Run as a nonroot numeric uid:gid (scratch has no /etc/passwd, which is fine).
COPY --from=builder --chown=65532:65532 /data /data
COPY --from=builder /minimail /usr/local/bin/minimail

USER 65532:65532
EXPOSE 1025 8025
VOLUME ["/data"]

ENTRYPOINT ["/usr/local/bin/minimail"]
# Overridable defaults: bind SMTP + HTTP on all interfaces, store in the volume.
CMD ["--smtp-bind", "0.0.0.0:1025", "--http-bind", "0.0.0.0:8025", "--root", "/data"]
