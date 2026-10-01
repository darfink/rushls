# syntax=docker/dockerfile:1.7

FROM rust:1.97-slim-trixie AS builder

WORKDIR /workspace

# gcc and the C library headers are for ring's vendored C/asm kernel, the only
# native code in the build. The binary links no system crypto library, and SRT
# is pure Rust (`rsrt`).
RUN apt-get update \
  && DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends \
    gcc \
    libc6-dev \
  && rm -rf /var/lib/apt/lists/*

COPY . .

# `.dockerignore` omits `.git`, so bake the commit in from the host:
#   docker build --build-arg GIT_SHA="$(git rev-parse --short=12 HEAD)" .
ARG GIT_SHA=
ENV GIT_SHA=$GIT_SHA

ARG TARGETPLATFORM
RUN --mount=type=cache,id=rushls-cargo-registry,target=/usr/local/cargo/registry,sharing=locked \
    --mount=type=cache,id=rushls-target-${TARGETPLATFORM},target=/workspace/target,sharing=locked \
    cargo build --release --locked -p rushls \
  && install -Dm755 target/release/rushls /usr/local/bin/rushls

# The runtime image has no package manager or coreutils to prepare
# directories with, so the writable state directory is made here and copied.
RUN install -d -o 65532 -g 65532 /state/var/lib/rushls

# Distroless `cc` carries exactly what the binary links (glibc, libgcc) plus CA
# roots for outbound HTTPS: the admission service, JWKS, and hooks. The `debug`
# variant adds a BusyBox shell at /busybox for `docker exec`; `nonroot` runs as
# UID/GID 65532.
FROM gcr.io/distroless/cc-debian13:debug-nonroot AS runtime

ARG GIT_SHA=
LABEL org.opencontainers.image.title="Rushls" \
      org.opencontainers.image.description="Live HLS and Low-Latency HLS origin for RTMP, SRT, and Media over QUIC" \
      org.opencontainers.image.source="https://github.com/darfink/rushls" \
      org.opencontainers.image.documentation="https://github.com/darfink/rushls#readme" \
      org.opencontainers.image.licenses="MIT" \
      org.opencontainers.image.revision=$GIT_SHA

# The builder copies the artifact out of its persistent Cargo target cache.
COPY --from=builder /usr/local/bin/rushls /usr/local/bin/rushls
COPY examples/container/rushls.toml /etc/rushls/rushls.toml

# The bundled configuration listens on container interfaces and permits publishing.
# Replace it with a bind mount or select another path with RUSHLS_CONFIG.
ENV RUSHLS_CONFIG=/etc/rushls/rushls.toml

# A writable home for DVR spill and recordings, owned by the runtime user. A
# named volume mounted here inherits that ownership; a bind mount must be
# writable by 65532. XDG_CACHE_HOME puts the default `disk.dir` here too.
COPY --from=builder --chown=65532:65532 /state/ /
ENV XDG_CACHE_HOME=/var/lib/rushls/cache
WORKDIR /var/lib/rushls

# RTMP/TCP, SRT/UDP, and HLS/HTTP, as the bundled configuration listens.
# RTMPS (1936/tcp), HTTPS, and MoQ (UDP) are off until configured with a certificate.
EXPOSE 1935/tcp 9000/udp 8080/tcp

# No privileged port is required. The base image already runs as 65532; the
# numeric identity is repeated so it holds even where /etc/passwd is absent.
USER 65532:65532

# Rushls handles SIGTERM itself, draining hooks and recordings for
# `shutdown_grace`; give `docker stop -t` longer than that.
ENTRYPOINT ["/usr/local/bin/rushls"]
