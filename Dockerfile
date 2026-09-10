# syntax=docker/dockerfile:1.7

FROM rust:1.97-slim-trixie AS builder

WORKDIR /workspace

# gcc is for ring's vendored C/asm kernel at build time. The crate does not
# link a system crypto library, and SRT is now pure Rust (`rsrt`).
RUN apt-get update \
  && DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends \
    build-essential \
    ca-certificates \
  && rm -rf /var/lib/apt/lists/*

COPY Cargo.toml Cargo.lock ./
COPY crates ./crates
COPY apps ./apps

# `.dockerignore` omits `.git`, so bake the commit in from the host:
#   docker build -f Dockerfile \
#     --build-arg GIT_SHA="$(git rev-parse --short=12 HEAD)" .
ARG GIT_SHA=
ENV GIT_SHA=$GIT_SHA

ARG TARGETPLATFORM
RUN --mount=type=cache,id=rushls-cargo-registry,target=/usr/local/cargo/registry,sharing=locked \
    --mount=type=cache,id=rushls-target-${TARGETPLATFORM},target=/workspace/target,sharing=locked \
    cargo build --release --locked -p rushls \
  && install -Dm755 target/release/rushls /usr/local/bin/rushls

FROM debian:trixie-slim AS runtime

ARG GIT_SHA=
LABEL org.opencontainers.image.revision=$GIT_SHA

RUN apt-get update \
  && DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends \
    ca-certificates \
  && rm -rf /var/lib/apt/lists/*

# The builder copies the artifact out of its persistent Cargo target cache.
COPY --from=builder /usr/local/bin/rushls /usr/local/bin/rushls
COPY rushls.toml /etc/rushls/rushls.toml

# The bundled file is the complete reference and carries the built-in defaults.
# Replace it with a bind mount or select another path with RUSHLS_CONFIG.
ENV RUSHLS_CONFIG=/etc/rushls/rushls.toml

# RTMP/TCP, SRT/UDP, and HLS/HTTP respectively.
EXPOSE 1935/tcp 9000/udp 8080/tcp

# No writable application directory or privileged port is required. A numeric
# identity also works in minimal Kubernetes environments without /etc/passwd.
USER 65532:65532

ENTRYPOINT ["/usr/local/bin/rushls"]
