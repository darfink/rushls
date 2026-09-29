<h1 align="center">
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset="docs/assets/rushls-dark.svg">
    <img src="docs/assets/rushls.svg" alt="Rushls" width="420">
  </picture>
</h1>
<p align="center"><strong>One focus: excellent HLS.</strong></p>
<p align="center">
  <a href="https://github.com/darfink/rushls/actions/workflows/ci.yaml"><img src="https://github.com/darfink/rushls/actions/workflows/ci.yaml/badge.svg" alt="Build and validation"></a>
  <a href="https://crates.io/crates/rushls"><img src="https://img.shields.io/crates/v/rushls.svg" alt="crates.io"></a>
  <a href="https://github.com/darfink/rushls/pkgs/container/rushls"><img src="https://img.shields.io/badge/container-GHCR-986044" alt="Container registry"></a>
  <a href="LICENSE"><img src="https://img.shields.io/badge/license-MIT-blue.svg" alt="MIT license"></a>
</p>

Rushls focuses on one job: serving HLS and low-latency HLS well.
It is an origin written in Rust, not an attempt to support every streaming protocol and workflow.
RTMP, SRT, and Media over QUIC are ingest paths into that HLS pipeline.
It preserves encoded media without transcoding. The publisher supplies every video variant and audio rendition.

## Why Rushls?

I built Rushls after testing open-source HLS servers for my own production workloads.
Every server I tested exhibited issues once I moved beyond trivial use cases.
That experience motivated a dedicated HLS origin, with attention to multitrack streams, DVR, captions, reconnects, and playback behavior.
This describes my tests and requirements, not a benchmark of every server available today.

Rushls is used in production today. Its development includes:

- More than 1,000 Rust library tests, plus integration, decoder, browser, and load tests.
- Apple's `mediastreamvalidator` and `hlsreport`, including a two-hour DVR authoring audit with published CI reports.
- Playback checks for real media, multiple renditions, and live-to-ended transitions.
- Several corrections to upstream hls.js code discovered during development: pending-part selection, fragment tracking, and ENDLIST state preservation.

The [hls.js corrections](tools/patches/hls.js/README.md) currently ship as a local patch intended for upstream submission; they are not yet merged upstream.
Validation reports retain [documented exceptions and compatibility limits](docs/ci-validation.md).
The goal is reliable HLS behavior in demanding workflows, supported by reproducible evidence.

```text
FFmpeg / GStreamer / OBS / MoQ publisher
              │ RTMP, MPEG-TS over SRT, or MoQ
              ▼
           Rushls ──── recording files
              │ HLS / LL-HLS over HTTP(S)
              ▼
         Player or CDN
```

Rushls supports multitrack publishing, rolling DVR, disk retention, recording, publisher authorization, JWT playback authorization, signed hooks, and Prometheus metrics.
It runs without system FFmpeg or SRT libraries. Publishing and validation tools are separate dependencies.

## Contents

- **🧭 Start here**
  - [Why Rushls?](#why-rushls)
  - [Supported protocols and codecs](#supported-protocols-and-codecs)
  - [Installation](#installation): [Cargo](#cargo), [binaries](#native-binaries), [Docker](#docker)
  - [Quick start](#quick-start)
  - [Configuration](#configuration)
- **📡 Publish and play**
  - [Publishing streams](#publishing-streams)
    - [FFmpeg](#ffmpeg)
    - [GStreamer](#gstreamer)
    - [OBS Studio](#obs-studio)
    - [Media over QUIC](#media-over-quic)
  - [Multiple variants, audio tracks, and subtitles](#multiple-variants-audio-tracks-and-subtitles)
  - [Playback and latency](#playback-and-latency)
- **🔧 Operate**
  - [DVR and resource limits](#dvr-and-resource-limits)
  - [Recording](#recording)
  - [Authentication and encryption](#authentication-and-encryption)
    - [Publisher admission](#publisher-admission)
    - [Playback authorization](#playback-authorization)
    - [TLS and transport encryption](#tls-and-transport-encryption)
  - [Production operation](#production-operation)
  - [Troubleshooting and compatibility](#troubleshooting-and-compatibility)
- **Contribute**
  - [Development](#development)
    - [Build from source](#build-from-source)
    - [Run checks](#run-checks)
  - [License](#license)

## Supported protocols and codecs

This table describes server ingestion and output, not decoder support in every player.
**Yes** means an implemented path. Footnotes identify restrictions and differences in validation coverage.

| Codec / media | RTMP / Enhanced RTMP | MPEG-TS over SRT | MoQ | HLS output |
| --- | --- | --- | --- | --- |
| H.264 / AVC | Yes | Yes | Yes [3] | CMAF |
| H.265 / HEVC | Enhanced RTMP | Yes | Yes [3] | CMAF |
| AV1 | Enhanced RTMP | AV1G mapping only [1] | Yes [3] | CMAF |
| AAC-LC | Yes | ADTS | Yes [3] | CMAF |
| HE-AAC / HE-AAC v2 | Explicit SBR/PS signaling [2] | ADTS limitations [2] | Configuration-dependent [2][3] | CMAF |
| Opus | Enhanced RTMP [4] | Mono/stereo [4] | Yes [3] | CMAF |
| FLAC | Enhanced RTMP, mono/stereo [4] | No | No | CMAF |
| Text captions | `onCaption` / `onTextData` [5] | No standalone subtitle ingest | No standalone subtitle ingest | WebVTT |
| Embedded CEA-608/708 | H.264 / HEVC [5] | H.264 / HEVC [5] | Not separately validated | In-band declarations [5] |
| MP3, AC-3, E-AC-3, VP9, VVC | No | No | No | No |

1. AV1 over MPEG-TS requires the GStreamer AV1G private-PES mapping, its descriptor, and an in-band sequence header.
2. HE-AAC requires explicit decoder signaling. Implicit SBR detection and AAC-LATM ingestion are unsupported. Do not assume every ADTS HE-AAC source works.
3. MoQ uses `moq-lite-05`, LOC or legacy Hang, and catalog decoder configuration. The native H.264/AAC publisher is tested. Browser publishing remains unverified.
4. Decoder tests cover FLAC and MPEG-TS Opus. Browser FLAC playback and live RTMP Opus have narrower validation coverage.
5. Script captions become WebVTT. Embedded captions remain in the video and receive HLS declarations. They are not converted into WebVTT.

SRT accepts MPEG-TS only, not Matroska, WebM, FLV, or MP4.
Rushls does not provide RTSP, WebRTC/WHIP, or RTMP/SRT playback endpoints.
H.264/AAC is the starting point for broad player compatibility.
See [ingest coverage](docs/feature-parity.md) and the [player matrix](docs/gap-player-matrix.md) for detailed evidence.

## Installation

### Cargo

Install Rust 1.97 or later and a C compiler, then install Rushls from crates.io:

```sh
cargo install rushls --locked
```

Cargo compiles the executable and installs it in `~/.cargo/bin`. Make sure this directory is on `PATH`.
See [Quick start](#quick-start) to create a local configuration and publish your first stream.

### Native binaries

The release workflow targets Linux and macOS on AMD64/ARM64, plus Windows AMD64.
A first application release is pending. The `ci-tools` prerelease contains validation tools, not Rushls binaries.
Check the [release page](https://github.com/darfink/rushls/releases) for application archives.

Archives include configuration examples, documentation, dependency notices, and SHA-256 checksums.
Linux builds require glibc 2.39 or later. The macOS matrix uses macOS 15.
Windows recording requires a filesystem with hard links, such as NTFS.
See [platform requirements and release procedures](docs/releases.md).

### Docker

CI publishes the Linux AMD64 image to [GitHub Container Registry](https://github.com/darfink/rushls/pkgs/container/rushls) after validation passes.
The `edge` tag follows validated main builds. Stable releases also publish `latest` and version tags such as `v0.1.0`.
For production, pin a released version or image digest.
Public image access is pending; the commands below require registry access until the package is public.

```sh
docker pull ghcr.io/darfink/rushls:edge
docker run --rm --name rushls \
  -p 127.0.0.1:1935:1935/tcp \
  -p 127.0.0.1:9000:9000/udp \
  -p 127.0.0.1:8080:8080/tcp \
  ghcr.io/darfink/rushls:edge
```

The image listens on container interfaces and permits unauthenticated publishing.
These port mappings expose it only on the host loopback interface.
For remote access, configure authorization and encryption before changing the mappings.

Mount a deployment configuration at `/etc/rushls/rushls.toml`:

```sh
docker run --rm --name rushls \
  -p 1935:1935/tcp -p 9000:9000/udp -p 8443:8443/tcp \
  --mount type=bind,src="$PWD/production.toml",dst=/etc/rushls/rushls.toml,readonly \
  --mount type=bind,src="$PWD/secrets",dst=/run/secrets,readonly \
  --mount type=bind,src="$PWD/data",dst=/var/lib/rushls \
  ghcr.io/darfink/rushls:edge
```

This second command requires your deployment configuration, certificates, secrets, and writable data directory.
The container runs as UID/GID `65532:65532`. Grant that identity access to its volumes.
MoQ also needs its configured UDP port, for example `-p 4433:4433/udp`.
See [deployment](docs/deployment.md) for storage and proxy examples.

## Quick start

Create a configuration that binds each listener to loopback, then start the installed executable:

```sh
cat > rushls.toml <<'TOML'
[ingest.rtmp]
listen = "127.0.0.1:1935"
[ingest.srt]
listen = "127.0.0.1:9000"
[http]
listen = "127.0.0.1:8080"
TOML
rushls --config rushls.toml
```

Release archives and source checkouts already contain this starter as `rushls.toml`.

In another terminal, publish an H.264/AAC MP4 file:

<!-- verify: {"id":"quickstart","stream":"demo","video":1,"audio":1} -->
```sh
ffmpeg -re -i input.mp4 -map 0:v:0 -map 0:a:0 \
  -c copy -f flv rtmp://127.0.0.1:1935/live/demo
```

Open this URL in an HLS-capable player:

```text
http://127.0.0.1:8080/live/demo/index.m3u8
```

Without publisher authorization, `/live/demo` identifies the stream.
With authorization, the admission service selects the final stream ID and can change its playback path.
A plain browser navigation is not a JavaScript player. Safari can use native HLS; other browsers typically need an HLS library.

Stream copy preserves the input codecs, timestamps, and keyframes.
If the file lacks compatible media or regular keyframes, use the [encoding recipe](docs/publishing.md#prepare-a-test-file).
The first playable playlist appears after discovery and sufficient media arrive.

## Configuration

[rushls.toml](rushls.toml) is the short local starter.
[rushls.example.toml](rushls.example.toml) lists every supported TOML field, with optional features commented out.
Do not uncomment the entire example: some fields require external services, and some alternatives are mutually exclusive.

Print the annotated example from the installed binary:

```sh
rushls --print-config-example > my-rushls.toml
rushls --config my-rushls.toml
```

The print command exits without loading configuration or reading secret files.
Its output matches the installed version.
Use the full executable path if `rushls` is not on `PATH`.

Values resolve in this order: compiled defaults, TOML, environment, then CLI arguments.
For example:

```sh
RUSHLS_HTTP_LISTEN=127.0.0.1:18080 rushls --config rushls.toml
rushls --config rushls.toml --http-listen=127.0.0.1:18080
```

TOML strings support `${NAME}` and `${NAME:-fallback}` interpolation.
Credentials accept a string, `"${VAR}"`, or `{ file = "/path" }`.
Unknown `RUSHLS_` variables produce warnings. Unknown TOML fields and CLI arguments fail startup.
Structured policy fields are not all available as environment overrides. `rushls --help` lists supported overrides.
`rushls --config rushls.toml --check` validates the file, prints listeners and the worst-case memory plan, and exits.

The bundled files bind to loopback. Compiled RTMP, SRT, and HTTP defaults bind to all IPv4 interfaces.
RTMP and HTTP also accept explicit IPv6 addresses. SRT currently requires IPv4.

Configuration changes require a restart, except supported certificate and key rotation.
See the [configuration guide](docs/config.md) for discovery paths and detailed field behavior.

## Publishing streams

The examples use the local configuration and one stream name per command.
File publishers use an H.264/AAC `input.mp4`; the GStreamer RTMP example generates live test media.
Run each publisher separately. All resulting playlists follow `/live/NAME/index.m3u8`.
Tested publisher versions and complete recipes appear in [publishing](docs/publishing.md).

### FFmpeg

RTMP:

<!-- verify: {"id":"ffmpeg-rtmp","stream":"ffmpeg","video":1,"audio":1} -->
```sh
ffmpeg -re -i input.mp4 -map 0:v:0 -map 0:a:0 \
  -c copy -f flv rtmp://127.0.0.1:1935/live/ffmpeg
```

MPEG-TS over SRT:

<!-- verify: {"id":"ffmpeg-srt","stream":"ffmpeg","video":1,"audio":1} -->
```sh
ffmpeg -re -i input.mp4 -map 0:v:0 -map 0:a:0 -c copy -f mpegts \
  'srt://127.0.0.1:9000?mode=caller&streamid=publish:live/ffmpeg&pkt_size=1316'
```

Quote SRT URLs so the shell does not interpret `&`.
Rushls is the SRT listener; the publisher is the caller.
The stream ID selects the resource and supplies an admission credential. It is separate from SRT encryption.

### GStreamer

RTMP from live test sources:

<!-- verify: {"id":"gstreamer-rtmp","stream":"synthetic","video":1,"audio":1} -->
```sh
gst-launch-1.0 -e \
  videotestsrc is-live=true ! video/x-raw,width=640,height=360,framerate=30/1 \
  ! x264enc tune=zerolatency key-int-max=60 bitrate=1200 \
  ! h264parse ! queue ! mux. \
  audiotestsrc is-live=true ! audio/x-raw,rate=48000,channels=2 \
  ! avenc_aac ! aacparse ! queue ! mux. \
  flvmux name=mux streamable=true ! \
  rtmpsink location=rtmp://127.0.0.1:1935/live/synthetic sync=true
```

MPEG-TS over SRT:

<!-- verify: {"id":"gstreamer-srt","stream":"gstreamer","video":1,"audio":1} -->
```sh
gst-launch-1.0 -e filesrc location=input.mp4 ! qtdemux name=d \
  d.video_0 ! queue ! h264parse ! mux. \
  d.audio_0 ! queue ! aacparse ! mux. \
  mpegtsmux name=mux alignment=7 ! \
  srtsink uri='srt://127.0.0.1:9000?mode=caller&streamid=publish:live/gstreamer' sync=true
```

These commands require the relevant transport, parser, and encoder plugins.
Direct MP4-to-FLV remuxing in GStreamer produced overlapping startup timestamps in our test; use FFmpeg for RTMP file publishing.
The [publishing guide](docs/publishing.md#gstreamer) includes plugin checks and the observed file-conversion limitation.

### OBS Studio

Select **Settings → Stream → Custom**:

| Field | Local value |
| --- | --- |
| Server | `rtmp://127.0.0.1:1935/live` |
| Stream key | `obs` |
| Video encoder | H.264 |
| Audio encoder | AAC |
| Keyframe interval | 2 seconds |

The playback URL is `http://127.0.0.1:8080/live/obs/index.m3u8`.
With admission enabled, use the stream key issued by your application.
This is the conventional RTMP setup; OBS multitrack output requires separate version-specific verification.

### Media over QUIC

MoQ ingest is optional. It accepts `moq-lite-05` over WebTransport or raw QUIC on one UDP port.
It uses the certificate and private key from `[tls]`:

```toml
[ingest.moq]
listen = "0.0.0.0:4433"

[tls]
cert = "/run/secrets/fullchain.pem"
key = "/run/secrets/private-key.pem"
```

A native `moq-cli 0.10.0` publisher can import MPEG-TS from FFmpeg:

```sh
ffmpeg -re -i input.mp4 -map 0:v:0 -map 0:a:0 -c copy -f mpegts - \
  | moq --client-connect https://origin.example.com:4433/ \
      --broadcast live/moq --client-version moq-lite-05 import ts
```

Use a hostname covered by the certificate and trusted by the publisher.
Do not assume another MoQ draft or media container is compatible.
Browser publishing remains unverified. See [MoQ ingestion](docs/moq-ingestion.md) for catalog requirements and local certificate testing.

## Multiple variants, audio tracks, and subtitles

A publication can carry multiple video and audio tracks.
Rushls exposes them through one HLS multivariant playlist.
Separate stream IDs remain separate streams; Rushls does not merge independent publishers into one ladder.

| Goal | Publisher input |
| --- | --- |
| Adaptive video | Multiple encoded video tracks with aligned keyframes |
| Alternate audio | Multiple audio tracks, with language metadata where available |
| Separate channels | Different publication names / admitted stream IDs |
| Subtitles | Supported caption messages present during discovery |

For example, publish two video sizes and one audio track over SRT:

<!-- verify: {"id":"ladder-srt","stream":"ladder","video":2,"audio":1} -->
```sh
ffmpeg -re -i input.mp4 \
  -filter_complex '[0:v]split=2[hi][lo];[lo]scale=320:180[small]' \
  -map '[hi]' -map '[small]' -map 0:a:0 \
  -c:v libx264 -preset veryfast -pix_fmt yuv420p \
  -g 60 -keyint_min 60 -sc_threshold 0 -bf 0 \
  -b:v:0 1200k -b:v:1 400k -c:a aac -b:a 128k \
  -f mpegts 'srt://127.0.0.1:9000?mode=caller&streamid=publish:live/ladder&pkt_size=1316'
```

This example assumes a 30 fps, 16:9 source. Its GOP is two seconds.
Use matching timestamps and keyframe boundaries across video tracks.
Rushls discovers the track set at startup. Adding tracks or changing decoder configuration mid-publication fails ingestion.

Enhanced RTMP also supports multitrack messages.
See [multitrack publishing](docs/publishing.md#multitrack-publishing) for the tested FFmpeg version, RTMP recipe, and alternate audio example.

RTMP `onCaption` and `onTextData` messages create a WebVTT rendition when discovered before the track set freezes.
Late messages cannot create a new subtitle track.
Embedded CEA-608/708 captions in H.264/HEVC stay in the video; they are not an automatic WebVTT conversion path.
Standalone `.srt` or `.vtt` uploads and MPEG-TS subtitle PIDs are not supported ingest paths.
For a runnable RTMP subtitle example, see [gst-captions publishing](docs/publishing.md#publish-captions-with-gst-captions).
The [gst-captions](https://github.com/darfink/gst-captions) plugin adds timed text or live transcription as RTMP caption messages.

## Playback and latency

Always start with the multivariant playlist:

```text
https://origin.example.com/live/demo/index.m3u8
```

Native Safari HLS and JavaScript HLS players use the same URL.
A CDN can sit between viewers and Rushls. It must preserve LL-HLS query parameters and support blocking playlist requests.
For cross-origin playback, configure CORS for the player origin.

A starting configuration for segment and part duration is:

```toml
[hls]
segment = { target = "6s", max = "2x" }
part = { target = "1s", max = "2x" }
hold_back = "3x"
scrubbing = true
```

`hold_back` is a live-edge floor, not an end-to-end latency guarantee.
Encoder buffering, keyframes, SRT latency, networks, and player behavior also contribute.
Rushls cannot insert keyframes. Admission can select larger targets within the configured bounds to fit the source.
Scrubbing adds I-frame playlists for compatible CMAF video.

The [deployment guide](docs/deployment.md#reverse-proxies-and-cdns) explains HTTP timeouts and caching.
The [player matrix](docs/gap-player-matrix.md) separates measured playback results from codec packaging support.

## DVR and resource limits

DVR lets viewers seek within the retained live window.
Time retention and byte capacity both limit that window.
Configure two hours of requested retention with disk spill:

```toml
[hls]
window = "2h"

[limits]
publishers = 16
streams = 32

[memory]
per_stream = "256MiB"

[disk]
per_stream = "8GiB"
dir = "/var/lib/rushls/dvr"
```

At 6 Mbps total across all tracks, two hours needs about 5.4 GB of media before overhead.
An 8 GiB allowance provides headroom for that example, not for every ladder.
Sum every retained video and audio track, not just the variant a viewer selects.

```text
media bytes ≈ total bitrate in bits/second × retained seconds ÷ 8
```

Capacity pressure can shorten the requested window.
`memory.per_stream` also covers cached manifests. In-flight media is covered by `memory.per_publisher`.
Disk-backed streams reserve part of their memory allowance for manifests.
These values are not a hard process RSS limit. Set `memory.total` to refuse publications once the committed budgets would exceed a node-wide ceiling.

Multiply per-stream budgets by the maximum retained stream count.
Finished or disconnected streams can still occupy retention capacity.
Monitor actual retention depth, storage availability, and process memory.

DVR is process-lifetime storage. A restart does not restore the old live catalog from disk.
For persistent copies, configure recording separately.
See [storage sizing](docs/deployment.md#storage-and-capacity).

## Recording

Recording writes completed segments to a filesystem:

```toml
[record]
dir = "/var/lib/rushls/recordings"
path = "{stream}/{publication}/{time:%Y/%m/%d}/{rendition}_{segment}.mp4"
queue_size = 128
max_pending = "256MiB"
```

Each media file contains its decoder configuration. Source random access and codec preroll still affect independent playback.
Files appear under their final names after the write completes. Existing files are not overwritten.
Use `{publication}` to separate repeated sessions.

Recording survives the live stream and is independent of `hls.window`.
Rushls does not serve these files as an archive or delete them through DVR retention.
Provide an external retention/upload workflow for the recording directory.
Queue overflow or disk errors report recording failures while live delivery continues.

The directory must be writable by the service identity.
Windows requires hard-link support; its directory metadata does not have the same power-loss durability guarantee as Unix.
See [recording operations](docs/deployment.md#recording-operations).

## Authentication and encryption

### Publisher admission

Configure an HTTP service to authorize each publisher:

```toml
[publish.auth]
url = "https://auth.example.com/v1/publish/admit"
token = { file = "/run/secrets/admission-token" }
timeout = "2s"
```

The service returns an allow/deny decision, a stream ID, and a principal.
It can also select a named local media profile.
An unavailable or invalid admission response fails closed.
See the [publisher API](docs/publisher-api.md) for the exact request and response contracts.

### Playback authorization

JWT playback authorization verifies viewer tokens locally:

```toml
[playback.auth]
jwks_url = "https://issuer.example.com/.well-known/jwks.json"
stream_claim = "stream"

[playback.auth.claims]
iss = "https://issuer.example.com"
aud = "rushls-origin"
```

Tokens must identify the authorized stream and satisfy the configured claims.
Viewers can supply a bearer token or the supported `token` query parameter.
Choose CDN cache rules that preserve authorization. The query-token form requires HLS v11 variable substitution (`EXT-X-DEFINE:QUERYPARAM`), which not every player supports.
See [playback authorization](docs/deployment.md#authorization).

### TLS and transport encryption

Serve HTTPS directly:

```toml
[http]
listen = "off"

[https]
listen = "0.0.0.0:8443"
version = { min = "1.3", max = "1.3" }

[tls]
cert = "/run/secrets/fullchain.pem"
key = "/run/secrets/private-key.pem"
```

Set `min = "1.2"` to allow TLS 1.2 clients alongside TLS 1.3.
The same `[tls]` certificate serves MoQ ingest.
Certificate rotation reloads valid replacements without restarting the service.
Certificate issuance and renewal remain external responsibilities.

RTMP has no native RTMPS listener. Use an external TCP TLS terminator when RTMPS is required, and enable `ingest.rtmp.proxy_protocol` so Rushls still sees each client address.
SRT uses its own passphrase-based encryption, not TLS:

```toml
[ingest.srt]
passphrase = { file = "/run/secrets/srt-passphrase" }
encryption = "aes256"
```

MoQ always uses QUIC TLS 1.3. HTTP TLS version bounds do not change QUIC.
See [encryption and certificates](docs/deployment.md#encryption-and-certificates).

## Production operation

Expose Prometheus metrics on a private listener:

```toml
[metrics]
listen = "127.0.0.1:9090"
token = { file = "/run/secrets/metrics-token" }
```

`/metrics` provides totals; `/metrics/streams` provides per-stream and session detail.
HTTP `/health/live` and `/health/ready` support process and readiness probes.
Keep metrics private and use the supplied [monitoring examples](examples/monitoring).

Signed hooks report lifecycle events and completed, fetchable segments:

```toml
[hook.operations]
url = "https://hooks.example.com/rushls"
events = ["session.started", "session.ended", "segment.ready"]
signing_secret = { file = "/run/secrets/hook-signing" }
```

Hooks contain JSON metadata and resource paths, not media payloads.
They do not extend retention. Consumers must fetch segments before expiry and tolerate retries.
See [hook signing and delivery](docs/publisher-api.md) and [metrics](docs/metrics.md).

Strict input validation is the default. It rejects timing violations and dependent segment starts.
`publish.strict = false` enables bounded recovery for supported input gaps, not arbitrary damaged media repair.
Rushls does not synthesize missing frames or audio.
See [input modes](docs/input-modes.md) for supported cases and limits.

Reconnects and publisher takeover are distinct operations.
`publish.takeover = true` allows a new publisher to replace an active one.
An orderly end can close playlists with `ENDLIST`; transport interruptions can preserve a reconnect opportunity.
Protocol-specific behavior appears in [deployment](docs/deployment.md#reconnects-and-shutdown).

Use SIGTERM on Unix, or Ctrl+C/Ctrl+Break on Windows, for graceful shutdown.
Set the supervisor's termination grace longer than the configured `shutdown_grace` duration.
Recording, hooks, and outstanding requests need time to drain.

## Troubleshooting and compatibility

| Symptom | First check |
| --- | --- |
| Publisher rejected | Admission response, stream identity, codec support, strict timing validation, and source keyframes |
| Missing variant or audio track | Publisher mappings, discovery-time track presence, and actual transport multitrack support |
| High latency | Encoder buffers/GOP, SRT latency, selected segment/part targets, and player live-edge policy |
| DVR shorter than requested | Aggregate bitrate, byte limits, retained stream count, and disk availability |
| Recording fails | Directory ownership, free space, queue pressure, and filesystem hard-link support |
| HTTP works but browser playback fails | CORS, HTTPS mixed content, player library, and browser codec support |
| MoQ connection rejected | `moq-lite-05`, certificate trust, UDP reachability, and catalog format |

The confirmed upstream hls.js 1.7.3 failure used rendition switching followed by `ENDLIST`, with no injected GAPs.
That combined case does not establish that switching or `ENDLIST` alone requires the patch.
Required Chrome validation uses a [local hls.js correction](tools/patches/hls.js/README.md) pending upstream submission.
This is a specific tested transition, not a claim that ordinary Chrome playback generally fails.

Safari and GStreamer have separate documented GAP-recovery limitations.
GStreamer clean-stream playback passes. GAP signaling does not guarantee seamless decoder recovery.
See [player compatibility](docs/releases.md#player-compatibility) and [known limitations](TODO.md).

## Development

### Build from source

Install Rust 1.97 or later and a C compiler for the `ring` dependency.

```sh
git clone https://github.com/darfink/rushls.git
cd rushls
cargo build --release --locked
./target/release/rushls --config rushls.toml
```

On Windows, the executable is `target\release\rushls.exe`.
Shell examples below use POSIX syntax. PowerShell requires its own line-continuation syntax.

### Run checks

```sh
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features --locked -- -D warnings
cargo test --workspace --all-features --locked
python3 -m unittest discover -s tools -p 'test_*.py'
```

CI [extracts and runs the tagged publishing examples](docs/ci-validation.md#executable-documentation) from this README and the publishing guide.
It checks each audio/video rendition and verifies the caption sample in served WebVTT.
Configuration examples also pass through the real loader and resolver.

External media, browser, and Apple checks require additional tools.
See [CI validation](docs/ci-validation.md), [load validation](docs/load-validation.md), and [architecture](docs/architecture.md).
All local dependencies reside in `crates/`; a fresh clone needs no sibling repositories.

## License

Rushls uses the [MIT license](LICENSE). Third-party components retain their own licenses.
