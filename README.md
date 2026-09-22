# Rushls

Rushls is a low-latency HLS origin written in Rust. It accepts live media through
RTMP, SRT, and Media over QUIC, then packages and serves CMAF and WebVTT.
It preserves encoded media without transcoding.

## Capabilities

- HLS and low-latency HLS with partial segments, delta playlists, and blocking reloads
- Enhanced RTMP, MPEG-TS over SRT, and optional WebTransport ingest
- Memory and disk retention, recording, and reconnect handling
- Publisher authorization, JWT playback authorization, and signed lifecycle hooks
- Rotating TLS certificates, Prometheus metrics, and graceful shutdown

See [feature coverage](docs/feature-parity.md) and [known limitations](TODO.md)
for protocol details and remaining work.

## Build and run

Install Rust 1.97 or later and a C compiler for the `ring` dependency.
The application requires no system FFmpeg or SRT libraries.

```sh
cargo build --release --locked
./target/release/rushls --config rushls.toml
```

The bundled `rushls.toml` binds listeners to loopback for local development.
It permits publishing without authentication.

Publish an H.264/AAC stream from an existing file with FFmpeg:

```sh
ffmpeg -re -i input.mp4 -c copy -f flv rtmp://127.0.0.1:1935/live/demo
```

Open `http://127.0.0.1:8080/live/demo/index.m3u8` in an HLS player.
FFmpeg is a publisher in this example; it is not a server dependency.

[rushls.example.toml](rushls.example.toml) documents every supported TOML field.
Its active settings match the local starter; optional features stay commented.
For public deployments, configure authorization, listener addresses, TLS, and capacity limits before exposing the service.

## Configuration

Values resolve in this order: defaults, TOML, environment variables, then CLI arguments.
For example, `RUSHLS_HTTP_LISTEN` overrides `[http] listen` in the TOML file.
Unknown `RUSHLS_` variables produce startup warnings. Unknown TOML fields and CLI arguments fail startup.
TOML strings support `${NAME}` and `${NAME:-fallback}` interpolation.

```sh
./target/release/rushls --help
./target/release/rushls --config /etc/rushls/rushls.toml
```

Print the same annotated example from the installed binary:

```sh
./target/release/rushls --print-config-example > rushls.toml
```

This command prints the embedded file and exits. It does not load configuration,
read secret files, or start listeners. Output matches the installed binary version.
`rushls.toml` is the short starter; `rushls.example.toml` is the complete annotated example.

See the [configuration guide](docs/config.md) and the
[configuration loader contract](crates/cc-config/README.md).

## Container

Build from the repository root:

```sh
docker build --build-arg GIT_SHA="$(git rev-parse --short=12 HEAD)" -t rushls .
docker run --rm -p 1935:1935 -p 9000:9000/udp -p 8080:8080 rushls
```

The image uses [the container example](examples/container/rushls.toml), which listens on container interfaces and permits publishing by default.
Mount a deployment configuration at `/etc/rushls/rushls.toml` to replace it.
The image runs as UID 65532. Recording and disk-retention volumes must permit writes by that user.

## Development

```sh
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features --locked -- -D warnings
cargo test --workspace --all-features --locked
python3 -m unittest discover -s tools -p 'test_*.py'
```

Some media validation tests are ignored by default. They require external decoders,
Apple validation tools, or a running browser. See the [validation guide](docs/validation-artifacts.md)
and [load validation](docs/load-validation.md).

## Repository layout

```text
src/       Ingest, media processing, packaging, delivery, and server runtime
crates/    Configuration, HTTP transport, hooks, TLS, and metrics helpers
tests/     Integration tests and media fixtures
docs/      Configuration, protocols, and validation findings
examples/  Monitoring examples and diagnostic programs
tools/     Local publishing, load, and playback validation tools
```

All local dependencies are included under `crates/`. A fresh clone requires no sibling repositories.
The [architecture guide](docs/architecture.md) explains the crate boundaries.
The [protocol resources](docs/resources/README.md) include the HLS draft used by the implementation.

## License

The manifests currently declare `UNLICENSED`. This repository does not grant an open-source license.
