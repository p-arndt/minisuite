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
# Must NOT carry a default: for predefined platform args, a Dockerfile default
# takes precedence over the value BuildKit injects, which would silently pin
# every platform to that default and produce an amd64 binary inside the arm64
# image.
ARG TARGETARCH

RUN apt-get update \
    && apt-get install -y --no-install-recommends \
       musl-tools \
       gcc-aarch64-linux-gnu \
       binutils \
    && rm -rf /var/lib/apt/lists/* \
    && rustup target add x86_64-unknown-linux-musl aarch64-unknown-linux-musl

# Cross-link the aarch64 musl target with the GNU cross compiler.
ENV CARGO_TARGET_AARCH64_UNKNOWN_LINUX_MUSL_LINKER=aarch64-linux-gnu-gcc

WORKDIR /build

COPY Cargo.toml Cargo.lock ./
COPY src ./src

# Map the Docker arch to the matching Rust musl target, then assert the binary
# we produced really is that architecture. Without the check, a build that
# resolves TARGETARCH wrongly ships an amd64 binary in the arm64 image and only
# fails at `docker run` time on an ARM host with "exec format error".
RUN set -eu; \
    case "$TARGETARCH" in \
        amd64) RUST_TARGET=x86_64-unknown-linux-musl; WANT_MACHINE="X86-64" ;; \
        arm64) RUST_TARGET=aarch64-unknown-linux-musl; WANT_MACHINE="AArch64" ;; \
        *)     echo "unsupported TARGETARCH: ${TARGETARCH:-<unset>}" >&2; exit 1 ;; \
    esac; \
    cargo build --release --target "$RUST_TARGET"; \
    cp "target/$RUST_TARGET/release/minicloak" /minicloak; \
    machine="$(readelf -h /minicloak | sed -n 's/^ *Machine: *//p')"; \
    echo "TARGETARCH=$TARGETARCH target=$RUST_TARGET machine=$machine"; \
    case "$machine" in \
        *"$WANT_MACHINE"*) ;; \
        *) echo "ERROR: built $machine binary for TARGETARCH=$TARGETARCH" >&2; exit 1 ;; \
    esac

# Pre-create the data directory so it lands in the runtime image owned by the
# nonroot user (scratch has no shell to mkdir at runtime). minicloak writes its
# signing key here so tokens survive a restart.
RUN mkdir -p /data

# ---- Runtime stage ------------------------------------------------------
# A fully static musl binary needs nothing at runtime (no libc, no certs for an
# inbound-only server), so we can ship it on an empty scratch image.
FROM scratch

ARG VERSION=dev
LABEL org.opencontainers.image.title="minicloak" \
      org.opencontainers.image.description="A tiny, dependency-free OpenID Connect provider" \
      org.opencontainers.image.source="https://github.com/p-arndt/minicloak" \
      org.opencontainers.image.version="$VERSION" \
      org.opencontainers.image.licenses="MIT"

# Run as a nonroot numeric uid:gid (scratch has no /etc/passwd, which is fine).
COPY --from=builder --chown=65532:65532 /data /data
COPY --from=builder /minicloak /usr/local/bin/minicloak

USER 65532:65532
EXPOSE 9500
VOLUME ["/data"]

ENTRYPOINT ["/usr/local/bin/minicloak"]
# Overridable defaults: bind on all interfaces (a container bound to 127.0.0.1
# is unreachable) and persist the RSA signing key in the volume.
CMD ["--bind", "0.0.0.0:9500", "--key", "/data/key.pem"]
