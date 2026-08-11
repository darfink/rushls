# syntax=docker/dockerfile:1.7

# Debian Trixie currently packages SRT 1.5.4, while Rushls deliberately
# requires 1.5.5 or newer. Keep this build isolated so the final image contains
# only SRT's shared crypto dependencies, not its compiler toolchain.
FROM debian:trixie-slim AS libsrt

ARG SRT_VERSION=1.5.5
ARG SRT_SHA256=c3518bc43a71b5289032395b2db4c3e09e73d78b54247d56c14553a503b491cf

RUN apt-get update \
  && DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends \
    build-essential \
    ca-certificates \
    cmake \
    curl \
    libssl-dev \
    pkg-config \
  && rm -rf /var/lib/apt/lists/*

RUN curl --fail --location --retry 5 --silent --show-error \
      "https://github.com/Haivision/srt/archive/refs/tags/v${SRT_VERSION}.tar.gz" \
      --output /tmp/srt.tar.gz \
  && echo "${SRT_SHA256}  /tmp/srt.tar.gz" | sha256sum --check --strict \
  && mkdir /tmp/srt \
  && tar --extract --gzip --file /tmp/srt.tar.gz --directory /tmp/srt --strip-components=1 \
  && cmake -S /tmp/srt -B /tmp/srt/build \
    -DCMAKE_BUILD_TYPE=Release \
    -DCMAKE_INSTALL_PREFIX=/opt/srt \
    -DENABLE_APPS=OFF \
    -DENABLE_EXAMPLES=OFF \
    -DENABLE_SHARED=OFF \
    -DENABLE_STATIC=ON \
    -DENABLE_UNITTESTS=OFF \
    -DUSE_ENCLIB=openssl \
  && cmake --build /tmp/srt/build --parallel \
  && cmake --install /tmp/srt/build

# Debian Trixie ships FFmpeg 7.1, whose FLV demuxer only understands Enhanced
# RTMP v1. GStreamer's eflvmux emits v2 multitrack packets for the rendition
# ladder, support for which landed in FFmpeg 8. Pin the same parser generation
# used by the host E2E tests instead of silently depending on the base image.
FROM debian:trixie-slim AS ffmpeg

ARG FFMPEG_VERSION=8.1.2
ARG FFMPEG_COMMIT=38b88335f99e76ed89ff3c93f877fdefce736c13

RUN apt-get update \
  && DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends \
    build-essential \
    ca-certificates \
    git \
    nasm \
    pkg-config \
  && rm -rf /var/lib/apt/lists/*

RUN git clone --depth 1 --branch "n${FFMPEG_VERSION}" \
      https://github.com/FFmpeg/FFmpeg.git /src/ffmpeg \
  && test "$(git -C /src/ffmpeg rev-parse HEAD)" = "${FFMPEG_COMMIT}" \
  && grep -q 'MultitrackTypeOneTrack' /src/ffmpeg/libavformat/flvdec.c

WORKDIR /src/ffmpeg

RUN ./configure \
    --prefix=/opt/ffmpeg \
    --disable-avdevice \
    --disable-avfilter \
    --disable-autodetect \
    --disable-debug \
    --disable-doc \
    --disable-programs \
    --disable-static \
    --enable-shared \
  && make -j"$(nproc)" \
  && make install

FROM rust:1.93-slim-trixie AS builder

WORKDIR /app

RUN apt-get update \
  && DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends \
    build-essential \
    clang \
    libclang-dev \
    libssl-dev \
    libzstd-dev \
    pkg-config \
    zlib1g-dev \
  && rm -rf /var/lib/apt/lists/*

COPY --from=libsrt /opt/srt /opt/srt
COPY --from=ffmpeg /opt/ffmpeg /opt/ffmpeg
ENV LD_LIBRARY_PATH=/opt/ffmpeg/lib \
    PKG_CONFIG_PATH=/opt/ffmpeg/lib/pkgconfig:/opt/srt/lib/pkgconfig

COPY Cargo.toml Cargo.lock build.rs ./
COPY src ./src
# Cargo.toml patches scuffle-rtmp to this audited local copy.
COPY vendor/scuffle-rtmp ./vendor/scuffle-rtmp

# `.dockerignore` omits `.git`, so bake the commit in from the host:
#   docker build --build-arg GIT_SHA="$(git rev-parse --short=12 HEAD)" .
ARG GIT_SHA=
ENV GIT_SHA=$GIT_SHA

ARG TARGETPLATFORM
RUN --mount=type=cache,id=rushls-cargo-registry,target=/usr/local/cargo/registry,sharing=locked \
    --mount=type=cache,id=rushls-target-${TARGETPLATFORM},target=/app/target,sharing=locked \
    cargo build --release --locked \
  && install -Dm755 target/release/rushls /usr/local/bin/rushls

FROM debian:trixie-slim AS runtime

ARG GIT_SHA=
LABEL org.opencontainers.image.revision=$GIT_SHA

RUN apt-get update \
  && DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends \
    ca-certificates \
    libssl3t64 \
    libstdc++6 \
    libzstd1 \
    zlib1g \
  && rm -rf /var/lib/apt/lists/*

COPY --from=ffmpeg /opt/ffmpeg /opt/ffmpeg
ENV LD_LIBRARY_PATH=/opt/ffmpeg/lib

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
