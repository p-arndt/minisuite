# syntax=docker/dockerfile:1

# minisuite — one Dockerfile, five stages.
#
#   docker build .                            -> the minisuite launcher image
#   docker build --target minicloak  -t ... . -> the OIDC provider alone
#   docker build --target minimail   -t ... . -> the SMTP sink alone
#   docker build --target minibucket -t ... . -> the S3 server alone
#
# The `minisuite` stage is deliberately LAST so a plain `docker build .` with no
# --target yields the all-in-one suite image.

# ---- Build stage --------------------------------------------------------
# Build fully static musl binaries so they can run on an empty scratch image.
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

# The whole workspace: one root manifest, one lockfile, every crate.
COPY Cargo.toml Cargo.lock ./
COPY crates ./crates

# Build every workspace binary in ONE cargo invocation (they share a dependency
# graph, so four separate builds would recompile the same crates four times),
# then assert each binary really is the requested architecture. Without the
# check, a build that resolves TARGETARCH wrongly ships an amd64 binary in the
# arm64 image and only fails at `docker run` time with "exec format error".
#
# The cache mounts are keyed by TARGETARCH so the amd64 and arm64 legs of a
# multi-arch build never share (and thus never invalidate) each other's target
# dir. Binaries are copied out to /out inside the same RUN, because a cache
# mount is not part of the resulting layer.
RUN --mount=type=cache,id=cargo-registry,target=/usr/local/cargo/registry,sharing=locked \
    --mount=type=cache,id=cargo-target-${TARGETARCH},target=/build/target \
    set -eu; \
    case "$TARGETARCH" in \
        amd64) RUST_TARGET=x86_64-unknown-linux-musl;  WANT_MACHINE="X86-64" ;; \
        arm64) RUST_TARGET=aarch64-unknown-linux-musl; WANT_MACHINE="AArch64" ;; \
        *)     echo "unsupported TARGETARCH: ${TARGETARCH:-<unset>}" >&2; exit 1 ;; \
    esac; \
    cargo build --release --workspace --target "$RUST_TARGET"; \
    mkdir -p /out; \
    for bin in minicloak minimail minibucket minisuite; do \
        src="target/$RUST_TARGET/release/$bin"; \
        if [ ! -f "$src" ]; then \
            echo "ERROR: $bin was not built (missing $src)" >&2; exit 1; \
        fi; \
        cp "$src" "/out/$bin"; \
        machine="$(readelf -h "/out/$bin" | sed -n 's/^ *Machine: *//p')"; \
        echo "TARGETARCH=$TARGETARCH target=$RUST_TARGET bin=$bin machine=$machine"; \
        case "$machine" in \
            *"$WANT_MACHINE"*) ;; \
            *) echo "ERROR: built $machine binary for TARGETARCH=$TARGETARCH" >&2; exit 1 ;; \
        esac; \
    done

# Pre-create the data directory so it lands in the runtime images owned by the
# nonroot user (scratch has no shell to mkdir at runtime).
RUN mkdir -p /data

# ---- Runtime: minicloak -------------------------------------------------
# A fully static musl binary needs nothing at runtime (no libc, no certs for an
# inbound-only server), so we can ship it on an empty scratch image.
FROM scratch AS minicloak

ARG VERSION=dev
LABEL org.opencontainers.image.title="minicloak" \
      org.opencontainers.image.description="A tiny, dependency-free OpenID Connect provider" \
      org.opencontainers.image.source="https://github.com/p-arndt/minisuite" \
      org.opencontainers.image.version="$VERSION" \
      org.opencontainers.image.licenses="MIT"

# Run as a nonroot numeric uid:gid (scratch has no /etc/passwd, which is fine).
COPY --from=builder --chown=65532:65532 /data /data
COPY --from=builder /out/minicloak /usr/local/bin/minicloak

USER 65532:65532
EXPOSE 9500
VOLUME ["/data"]

ENTRYPOINT ["/usr/local/bin/minicloak"]
# Overridable defaults: bind on all interfaces (a container bound to 127.0.0.1
# is unreachable) and persist the RSA signing key in the volume.
CMD ["--bind", "0.0.0.0:9500", "--key", "/data/key.pem"]

# ---- Runtime: minimail --------------------------------------------------
FROM scratch AS minimail

ARG VERSION=dev
LABEL org.opencontainers.image.title="minimail" \
      org.opencontainers.image.description="A tiny dev SMTP sink with a web UI + JSON API" \
      org.opencontainers.image.source="https://github.com/p-arndt/minisuite" \
      org.opencontainers.image.version="$VERSION" \
      org.opencontainers.image.licenses="MIT"

COPY --from=builder --chown=65532:65532 /data /data
COPY --from=builder /out/minimail /usr/local/bin/minimail

USER 65532:65532
EXPOSE 1025 8025
VOLUME ["/data"]

ENTRYPOINT ["/usr/local/bin/minimail"]
# Overridable defaults: bind SMTP + HTTP on all interfaces, store in the volume.
CMD ["--smtp-bind", "0.0.0.0:1025", "--http-bind", "0.0.0.0:8025", "--root", "/data"]

# ---- Runtime: minibucket ------------------------------------------------
FROM scratch AS minibucket

ARG VERSION=dev
LABEL org.opencontainers.image.title="minibucket" \
      org.opencontainers.image.description="A tiny, dependency-free S3-compatible object storage server" \
      org.opencontainers.image.source="https://github.com/p-arndt/minisuite" \
      org.opencontainers.image.version="$VERSION" \
      org.opencontainers.image.licenses="MIT"

COPY --from=builder --chown=65532:65532 /data /data
COPY --from=builder /out/minibucket /usr/local/bin/minibucket

USER 65532:65532
EXPOSE 9000
VOLUME ["/data"]

ENTRYPOINT ["/usr/local/bin/minibucket"]
# Overridable defaults: bind on all interfaces and store data in the volume.
CMD ["--bind", "0.0.0.0:9000", "--root", "/data"]

# ---- Runtime: minisuite (default target) --------------------------------
# All three servers in one process, plus a landing page on :9900. Each service
# keeps its state under <data>/<service>, so a single /data volume is enough.
FROM scratch AS minisuite

ARG VERSION=dev
LABEL org.opencontainers.image.title="minisuite" \
      org.opencontainers.image.description="minicloak + minimail + minibucket: a tiny local dev backend suite in one process" \
      org.opencontainers.image.source="https://github.com/p-arndt/minisuite" \
      org.opencontainers.image.version="$VERSION" \
      org.opencontainers.image.licenses="MIT"

COPY --from=builder --chown=65532:65532 /data /data
COPY --from=builder /out/minisuite /usr/local/bin/minisuite

USER 65532:65532
# minicloak, minibucket, minimail SMTP, minimail HTTP, landing page.
EXPOSE 9500 9000 1025 8025 9900
VOLUME ["/data"]

ENTRYPOINT ["/usr/local/bin/minisuite"]
# Overridable defaults: state in the volume, and bind every service on 0.0.0.0
# (the in-process default of 127.0.0.1 is unreachable from outside a container).
CMD ["--data", "/data", "--bind-all"]
