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

Rushls is a live streaming origin that does one thing: **HLS and Low-Latency HLS, the way Apple specifies them.**

There are plenty of open-source media servers. Most treat HLS as one output among many,
and few produce HLS that passes Apple's validation once a stream carries more than one video and one audio track.
Rushls is built for exactly that case. Publish **one** RTMP or SRT stream that carries an encoding ladder,
alternate audio languages, and captions, and Rushls serves it as **one adaptive HLS presentation**:
viewers switch bitrate, pick a language, and turn on subtitles.

- **Adaptive bitrate from a single publication.** Multiple video renditions, alternate audio tracks, WebVTT subtitles, and CEA-608/708 caption declarations.
- **Low-Latency HLS.** Partial segments, blocking playlist reload, delta updates, preload hints, and rendition reports.
- **Checked by Apple's own tools.** Every main-branch build runs Apple's `mediastreamvalidator` and `hlsreport`, including a two-hour live DVR audit. A failure blocks the release.
- **DVR, scrubbing, and recording.** A rolling window in memory with disk spill, I-frame playlists for fast seeking, and recording of every segment to disk.
- **CDN-ready.** `Cache-Control` follows the HLS specification's caching recommendations.
- **Built for operation.** Publisher admission webhook, JWT playback authorization, signed lifecycle hooks, and Prometheus metrics.
- **One self-contained binary.** No FFmpeg, no libsrt, nothing else to install.

Rushls packages media; it does not transcode. Your encoder produces the renditions, and Rushls turns them into HLS.

```text
OBS / FFmpeg / GStreamer / MoQ publisher
              │ RTMP, SRT, or Media over QUIC
              ▼
           Rushls ──── recordings on disk
              │ HLS / LL-HLS over HTTP(S)
              ▼
      CDN, then players
```

## Contents

- [Install](#install)
- [Quick start](#quick-start)
- [Configuration](#configuration)
- [Publishing](#publishing): [FFmpeg](#ffmpeg), [GStreamer](#gstreamer), [OBS Studio](#obs-studio), [Media over QUIC](#media-over-quic)
- [Renditions, audio tracks, and subtitles](#renditions-audio-tracks-and-subtitles)
- [Supported codecs](#supported-codecs)
- [HLS features](#hls-features)
- [Apple HLS Authoring Specification](#apple-hls-authoring-specification)
- [CDN and caching](#cdn-and-caching)
- [DVR and recording](#dvr-and-recording)
- [Security](#security)
- [Operations](#operations)
- [Advanced](#advanced)
- [Troubleshooting](#troubleshooting)
- [Development](#development)

## Install

**Binary.** Download an archive for Linux, macOS, or Windows from the [releases page](https://github.com/darfink/rushls/releases), extract it, and run `rushls`.
The first application release is still pending; until then, use Cargo.

**Cargo.** Install Rust 1.97 or later and a C compiler, then:

```sh
cargo install rushls --locked
```

**Docker.**

```sh
docker run --rm -p 1935:1935 -p 9000:9000/udp -p 8080:8080 ghcr.io/darfink/rushls:edge
```

The image is not public yet; until it is, pulling it requires registry access.

Linux binaries need glibc 2.39 or later. See [releases](docs/releases.md) for platforms and image tags.

## Quick start

Start the server. It runs without a configuration file:

```sh
rushls
```

Publish an H.264/AAC file from another terminal:

<!-- verify: {"id":"quickstart","stream":"demo","video":1,"audio":1} -->
```sh
ffmpeg -re -i input.mp4 -map 0:v:0 -map 0:a:0 \
  -c copy -f flv rtmp://127.0.0.1:1935/live/demo
```

Play it in Safari, VLC, or any HLS player:

```text
http://127.0.0.1:8080/live/demo/index.m3u8
```

The playlist appears once Rushls has seen enough media to plan segments, usually a few seconds.
Browsers other than Safari need a JavaScript player such as hls.js.
If the file has irregular keyframes, use the [encoding recipe](docs/publishing.md#prepare-a-test-file).

> [!WARNING]
> Without a configuration file, Rushls listens on all interfaces (RTMP 1935, SRT 9000, HTTP 8080) and **anyone who can reach it may publish**.
> That is fine on a laptop. Before exposing it, set up [publisher admission](#publisher-admission).

## Configuration

Write the annotated example, which lists every setting with its default, and edit what you need:

```sh
rushls --print-config-example > rushls.toml
rushls --check     # validate, show listeners and the memory plan, then exit
rushls
```

Rushls finds its configuration from `--config`, then `RUSHLS_CONFIG`, then `./rushls.toml`, then the platform's configuration directory.
With Docker, mount it over the bundled one:

```sh
docker run --rm -p 1935:1935 -p 9000:9000/udp -p 8080:8080 \
  -v "$PWD/rushls.toml:/etc/rushls/rushls.toml:ro" ghcr.io/darfink/rushls:edge
```

Most settings can also come from the environment or the command line, which win over the file (`rushls --help` lists them):

```sh
RUSHLS_HTTP_LISTEN=127.0.0.1:18080 rushls
rushls --http-listen 127.0.0.1:18080
```

Profiles, hooks, recording, and playback claims are file-only.
Secrets accept a string, `"${VAR}"`, or `{ file = "/run/secrets/name" }`.
Unknown settings stop startup instead of being ignored. Configuration changes need a restart, except rotated certificates and JWKS keys.
See the [configuration guide](docs/config.md), the [full reference](docs/config-reference.md), and [deployment](docs/deployment.md) for containers, storage, and proxies.

## Publishing

Every publisher below produces a playlist at `/live/NAME/index.m3u8`.
File examples use an H.264/AAC `input.mp4`. Tested versions and more recipes are in the [publishing guide](docs/publishing.md).

### FFmpeg

RTMP:

<!-- verify: {"id":"ffmpeg-rtmp","stream":"ffmpeg","video":1,"audio":1} -->
```sh
ffmpeg -re -i input.mp4 -map 0:v:0 -map 0:a:0 \
  -c copy -f flv rtmp://127.0.0.1:1935/live/ffmpeg
```

SRT (MPEG-TS). Quote the URL so the shell leaves `&` alone:

<!-- verify: {"id":"ffmpeg-srt","stream":"ffmpeg","video":1,"audio":1} -->
```sh
ffmpeg -re -i input.mp4 -map 0:v:0 -map 0:a:0 -c copy -f mpegts \
  'srt://127.0.0.1:9000?mode=caller&streamid=publish:live/ffmpeg&pkt_size=1316'
```

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

SRT from a file:

<!-- verify: {"id":"gstreamer-srt","stream":"gstreamer","video":1,"audio":1} -->
```sh
gst-launch-1.0 -e filesrc location=input.mp4 ! qtdemux name=d \
  d.video_0 ! queue ! h264parse ! mux. \
  d.audio_0 ! queue ! aacparse ! mux. \
  mpegtsmux name=mux alignment=7 ! \
  srtsink uri='srt://127.0.0.1:9000?mode=caller&streamid=publish:live/gstreamer' sync=true
```

For RTMP from a file, use FFmpeg: GStreamer's MP4-to-FLV remux produced overlapping startup timestamps in our tests.

### OBS Studio

In **Settings → Stream**, choose **Custom**:

| Field | Value |
| --- | --- |
| Server | `rtmp://127.0.0.1:1935/live` |
| Stream key | `obs` |
| Video encoder | H.264 |
| Audio encoder | AAC |
| Keyframe interval | 2 seconds |

Play `http://127.0.0.1:8080/live/obs/index.m3u8`. With admission enabled, use the stream key your application issues.

### Media over QUIC

MoQ ingest is off by default. It needs a UDP port and a TLS certificate:

```toml
[ingest.moq]
listen = "0.0.0.0:4433"

[tls]
cert = "/run/secrets/fullchain.pem"
key = "/run/secrets/private-key.pem"
```

Publish with `moq-cli` 0.10.0:

```sh
ffmpeg -re -i input.mp4 -map 0:v:0 -map 0:a:0 -c copy -f mpegts - \
  | moq --client-connect https://origin.example.com:4433/ \
      --broadcast live/moq --client-version moq-lite-05 import ts
```

Rushls speaks `moq-lite-05` only. See [MoQ ingestion](docs/moq-ingestion.md) for catalog requirements and local certificates.

## Renditions, audio tracks, and subtitles

This is what Rushls is for. One publication can carry several video tracks, several audio tracks, and captions.
Rushls serves them together from one multivariant playlist, so players can switch between them.

| You want | The publisher sends |
| --- | --- |
| Adaptive bitrate | Several video tracks with aligned keyframes |
| Alternate languages | Several audio tracks, with language metadata |
| Subtitles | RTMP `onCaption` / `onTextData` messages, served as WebVTT |
| Closed captions | CEA-608/708 inside H.264/HEVC, declared in the playlist |

Two video sizes and one audio track over SRT:

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

This assumes a 30 fps source, so `-g 60` gives a keyframe every 2 seconds in both renditions.
Enhanced RTMP carries multiple tracks too; see [multitrack publishing](docs/publishing.md#multitrack-publishing) for RTMP and alternate audio recipes.

Subtitles: caption messages become a WebVTT rendition when they are present at startup.
The [gst-captions](https://github.com/darfink/gst-captions) plugin sends timed text or live transcription as RTMP captions;
see [publishing captions](docs/publishing.md#publish-captions-with-gst-captions).

Rushls fixes the set of tracks when a publication starts. Adding a track, or changing a codec configuration mid-stream, ends the publication.
Separate stream names stay separate streams; Rushls does not merge publishers into one ladder.

## Supported codecs

Input protocols and codecs. Output is always HLS with CMAF (fragmented MP4) segments, and WebVTT for subtitles.

| | RTMP | SRT | MoQ |
| --- | :---: | :---: | :---: |
| H.264 / AVC | ✅ | ✅ | ✅ |
| H.265 / HEVC | ✅ <sup>1</sup> | ✅ | ✅ |
| AV1 | ✅ <sup>1</sup> | ✅ <sup>2</sup> | ✅ |
| AAC-LC | ✅ | ✅ | ✅ |
| HE-AAC v1 / v2 | ✅ <sup>3</sup> | ✅ <sup>3</sup> | ✅ <sup>3</sup> |
| Opus | ✅ <sup>1</sup> | ✅ <sup>4</sup> | ✅ |
| FLAC | ✅ <sup>1 4</sup> | ❌ | ❌ |
| Captions → WebVTT | ✅ | ❌ | ❌ |
| CEA-608/708 in video | ✅ | ✅ | ❔ <sup>5</sup> |
| MP3, AC-3, E-AC-3, VP9, VVC | ❌ | ❌ | ❌ |

1. Requires Enhanced RTMP.
2. Only with the GStreamer AV1 mapping (AV1G private PES) and an in-band sequence header.
3. Only with explicit SBR/PS signalling. Implicit HE-AAC and AAC-LATM are not detected.
4. Mono and stereo.
5. Not validated.

SRT carries MPEG-TS only. MoQ uses `moq-lite-05`. Rushls has no RTSP, WebRTC, or WHIP ingest, and no RTMP or SRT playback.
For the widest player support, publish H.264 and AAC-LC. Player support for HEVC, AV1, Opus, and FLAC in HLS varies; see the [player matrix](docs/gap-player-matrix.md).

## HLS features

| Feature | Tags and parameters | |
| --- | --- | :---: |
| fMP4 / CMAF segments | `EXT-X-MAP`, `EXT-X-INDEPENDENT-SEGMENTS` | ✅ |
| Adaptive bitrate | `EXT-X-STREAM-INF` with `CODECS`, `RESOLUTION`, `FRAME-RATE`, `AVERAGE-BANDWIDTH`, `VIDEO-RANGE` | ✅ |
| Alternate audio | `EXT-X-MEDIA` `TYPE=AUDIO` with `LANGUAGE`, `CHANNELS`, `DEFAULT`, `AUTOSELECT` | ✅ |
| WebVTT subtitles | `EXT-X-MEDIA` `TYPE=SUBTITLES` | ✅ |
| Closed captions | `EXT-X-MEDIA` `TYPE=CLOSED-CAPTIONS` with `INSTREAM-ID` | ✅ |
| Partial segments | `EXT-X-PART`, `EXT-X-PART-INF`, `EXT-X-PRELOAD-HINT` | ✅ |
| Blocking playlist reload | `EXT-X-SERVER-CONTROL` `CAN-BLOCK-RELOAD`, `_HLS_msn`, `_HLS_part` | ✅ |
| Delta playlist updates | `CAN-SKIP-UNTIL`, `_HLS_skip`, `EXT-X-SKIP` | ✅ |
| Rendition reports | `EXT-X-RENDITION-REPORT` | ✅ |
| Live-edge hold back | `HOLD-BACK`, `PART-HOLD-BACK` | ✅ |
| Scrubbing / trick play | `EXT-X-I-FRAME-STREAM-INF`, `EXT-X-I-FRAMES-ONLY`, `EXT-X-BYTERANGE` | ✅ |
| Wall-clock time | `EXT-X-PROGRAM-DATE-TIME` | ✅ |
| Gaps and reconnects | `EXT-X-GAP`, `EXT-X-DISCONTINUITY`, `EXT-X-DISCONTINUITY-SEQUENCE` | ✅ |
| DVR window and end of stream | `EXT-X-MEDIA-SEQUENCE`, `EXT-X-ENDLIST` | ✅ |
| Token in query string | `EXT-X-DEFINE:QUERYPARAM` | ✅ |
| HTTP/2 | Over HTTPS, negotiated with ALPN | ✅ |
| Content steering | `EXT-X-CONTENT-STEERING` | ❌ |
| Date ranges, ad markers, interstitials | `EXT-X-DATERANGE` | ❌ |
| Encryption and DRM | `EXT-X-KEY`, `EXT-X-SESSION-KEY` | ❌ |
| Session data | `EXT-X-SESSION-DATA` | ❌ |
| MPEG-TS segments | | ❌ |

## Apple HLS Authoring Specification

The [HLS Authoring Specification for Apple Devices](https://developer.apple.com/documentation/http-live-streaming/hls-authoring-specification-for-apple-devices)
splits the work between the packager and the encoder.

Rushls handles the packaging side: CMAF segments with a 6-second target, 1-second parts, I-frame playlists,
complete `EXT-X-STREAM-INF` attributes, independent segments, and program date-time.
It does not implement content steering, encryption, or interstitials.

The encoder side is yours. A publication that follows these settings gives a presentation that meets the specification:

| Setting | Recommendation |
| --- | --- |
| Keyframes | Every 2 seconds, at the same timestamps in every rendition |
| Scene cuts | Disable scene-cut keyframes (`-sc_threshold 0`), or force the 2-second grid with `-force_key_frames 'expr:gte(t,n_forced*2)'` |
| Video codec | H.264 for compatibility; HEVC for HDR and 4K |
| Ladder | Several rungs, for example 1920×1080 at 6 Mbps, 1280×720 at 3 Mbps, 960×540 at 2 Mbps, 640×360 at 365 kbps |
| Frame rate | The source frame rate, the same in every rung |
| Audio | AAC-LC stereo at 48 kHz, around 128 kbps |

The [renditions example](#renditions-audio-tracks-and-subtitles) shows the FFmpeg options for aligned keyframes.

## CDN and caching

Put a CDN between Rushls and your viewers. Rushls sets `Cache-Control` on every response,
following the recommendations in [RFC 8216bis Appendix B](https://datatracker.ietf.org/doc/html/draft-pantos-hls-rfc8216bis#appendix-B),
so a CDN that honours origin headers needs no per-path TTLs:

| Response | Cached for |
| --- | --- |
| Playlist, blocking reload (`_HLS_msn` in the URL) | 6 × target duration |
| Playlist, live edge | ½ × target duration |
| Init section, segment, part (`.mp4`, `.m4s`, `.vtt`) | 6 × target duration, `immutable` |
| Not found, request with an `_HLS_` directive | 4 × target duration |
| Not found, otherwise | 1 × target duration |

Lifetimes under one second become `no-cache`. With playback authorization on, `public` is left out,
so shared caches do not store a response fetched with an `Authorization` header.

**Cloudflare.** Cloudflare decides what to cache by file extension unless a Cache Rule says otherwise. For the stream paths:

1. Add a Cache Rule that makes them eligible for cache, with **Edge TTL** set to use the origin's `Cache-Control` header.
2. Keep the query string in the cache key (the default). `_HLS_msn`, `_HLS_part`, and `_HLS_skip` name different playlist states.
3. Optionally set `http.public_url` to the Cloudflare hostname, so the startup log prints the viewer-facing playlist URL.
4. Check Cloudflare's terms for your plan: they restrict serving video through the standard CDN on some plans.

Any CDN must also hold blocking playlist requests open, support Range requests, and never share a playlist that carries one viewer's token.
See [reverse proxies and CDNs](docs/deployment.md#reverse-proxies-and-cdns) for Nginx and CORS.

## DVR and recording

**DVR** lets viewers seek back within the live stream. Keep two hours, spilling to disk when memory fills:

```toml
[hls]
window = "2h"

[memory]
per_stream = "256MiB"

[disk]
dir = "/var/lib/rushls/dvr"
per_stream = "8GiB"
```

Size the disk for every track, not only the one a viewer watches: bytes ≈ total bitrate × seconds ÷ 8.
Two hours at 6 Mbps across all renditions is about 5.4 GB. If a limit is reached, the window gets shorter.
DVR lasts as long as the process; a restart starts fresh. [Storage and capacity](docs/deployment.md#storage-and-capacity) covers sizing.

**Recording** writes every completed segment to disk, independent of the DVR window:

```toml
[record]
dir = "/var/lib/rushls/recordings"
path = "{stream}/{publication}/{time:%Y/%m/%d}/{rendition}_{segment}.mp4"
```

Each file is complete when its final name appears; in-progress files are named `.rushls-*.tmp`.
Rushls does not upload recordings itself. To move them to S3 or any other object store, run [rclone](https://rclone.org) on a timer:

```sh
rclone move /var/lib/rushls/recordings s3:my-bucket/recordings \
  --exclude '.rushls-*' --min-age 1m
```

If disk or queue limits are hit, recording drops segments and reports it, and live delivery continues.
See [recording operations](docs/deployment.md#recording-operations).

## Security

### Publisher admission

Let your own service decide who may publish, and under which stream ID:

```toml
[publish.auth]
url = "https://auth.example.com/v1/publish/admit"
token = { file = "/run/secrets/admission-token" }
timeout = "2s"
```

Rushls sends the stream key and client details; the service answers allow or deny, with a stream ID and optionally a media profile.
If the service is unreachable, publishing is refused. See the [publisher API](docs/publisher-api.md).

### Playback authorization

Verify viewer JWTs locally against a JWKS:

```toml
[playback.auth]
jwks_url = "https://issuer.example.com/.well-known/jwks.json"
stream_claim = "stream"

[playback.auth.claims]
iss = "https://issuer.example.com"
aud = "rushls-origin"
```

Viewers send the token as a bearer header or a `token` query parameter. The query form relies on
`EXT-X-DEFINE:QUERYPARAM`, which not every player supports. See [authorization](docs/deployment.md#authorization).

### TLS

Serve HTTPS, and HTTP/2, directly:

```toml
[http]
listen = "off"

[https]
listen = "0.0.0.0:8443"

[tls]
cert = "/run/secrets/fullchain.pem"
key = "/run/secrets/private-key.pem"
```

Rushls reloads rotated certificates without a restart. The same certificate serves MoQ.
SRT uses its own encryption: set `ingest.srt.passphrase`.
For RTMPS, put a TCP TLS terminator in front of the RTMP port, and see [client addresses](#client-addresses-behind-a-proxy).
See [encryption and certificates](docs/deployment.md#encryption-and-certificates).

## Operations

Expose Prometheus metrics on a private listener, and send lifecycle events to your service:

```toml
[metrics]
listen = "127.0.0.1:9090"
token = { file = "/run/secrets/metrics-token" }

[hook.operations]
url = "https://hooks.example.com/rushls"
events = ["session.started", "session.ended", "stream.available", "segment.ready"]
signing_secret = { file = "/run/secrets/hook-signing" }

[log]
format = "json"
```

- `/metrics` has totals, `/metrics/streams` has per-stream detail. `/health/live` and `/health/ready` serve probes. See [metrics](docs/metrics.md) and the [monitoring examples](examples/monitoring).
- Hooks are signed CloudEvents with metadata and paths, not media. See [hooks](docs/publisher-api.md#lifecycle-hooks).
- `[log] level` sets how much Rushls logs; `RUST_LOG` overrides it for dependencies too.
- Stop with SIGTERM (Ctrl+C on Windows). Give the supervisor more time than `shutdown_grace`, so hooks and recordings can drain.

## Advanced

### Irregular keyframe intervals

Rushls fixes a segment schedule from the first keyframes it sees. Every later segment boundary needs a keyframe near its planned time.
An encoder whose keyframe interval varies, such as a variable frame rate source or scene-change keyframes, eventually misses one, and the publication ends with:

```text
no keyframe for segment boundary at 16s (>= 4.12s late, tolerance 0s); set segment.tolerance >= 5s or fix the encoder keyframe interval
```

Fix the encoder if you can. Otherwise, allow the boundary to move:

```toml
[hls]
segment = { target = "6s", max = "2x", tolerance = "2s" }
```

The cost applies to the whole publication: `EXT-X-TARGETDURATION` grows by twice the tolerance (6 s becomes 10 s here),
and players without Low-Latency HLS sit further behind the live edge. See [irregular keyframe intervals](docs/config.md#hls).

### Scrubbing

Scrubbing is the thumbnail preview you see while dragging the seek bar. Rushls serves an I-frame playlist for each video rendition,
pointing at the keyframes inside existing segments, so players can show it without downloading whole segments. It is on by default; turn it off with `hls.scrubbing = false`.
With a 2-second GOP there is one frame every 2 seconds; a 1-second GOP gives denser previews.

### Latency

```toml
[hls]
segment = "6s"
part = "1s"
hold_back = "3x"
```

Low-Latency HLS players follow parts, so `part` and `hold_back` (in parts) set how close to live they play.
Encoder buffering, the keyframe interval, SRT latency, and the player add to that. Rushls cannot insert keyframes.

### Input validation and takeover

Rushls rejects timing errors in the input by default. `publish.strict = false` allows bounded recovery from gaps; it does not repair damaged media or invent frames.
See [input modes](docs/input-modes.md).
`publish.takeover = true` lets a new publisher replace the current one under the same stream ID. See [reconnects and shutdown](docs/deployment.md#reconnects-and-shutdown).

### Client addresses behind a proxy

Behind a TLS terminator or load balancer, every RTMP publisher appears to come from the proxy.
Enable the PROXY protocol (v1 or v2) so Rushls sees the real client, and optionally limit publishers per client:

```toml
[ingest.rtmp]
listen = "0.0.0.0:1935"
proxy_protocol = true

[limits]
publishers_per_address = 16
```

The per-address limit covers RTMP, SRT, and MoQ together, and counts IPv6 clients per /64.

## Troubleshooting

| Symptom | Check |
| --- | --- |
| Publisher rejected | Admission response, codec support, timing validation, and source keyframes |
| "no keyframe for segment boundary" | The encoder's keyframe interval; see [irregular keyframe intervals](#irregular-keyframe-intervals) |
| A rendition or audio track is missing | Publisher track mapping, and that every track is present at startup |
| High latency | Encoder buffering and GOP, SRT latency, segment and part targets, and the player |
| DVR shorter than requested | Total bitrate, memory and disk limits, and free disk space |
| Recording fails | Directory ownership, free space, and hard-link support on the filesystem |
| HTTP works but the browser does not play | CORS, mixed HTTP/HTTPS content, the player library, and browser codec support |
| MoQ connection rejected | `moq-lite-05`, certificate trust, UDP reachability, and catalog format |

Player notes: hls.js 1.7.3 fails one tested case, a rendition switch followed by `ENDLIST`; CI uses a [local hls.js correction](tools/patches/hls.js/README.md) pending upstream submission.
Safari and GStreamer have their own GAP-recovery limits. See the [player matrix](docs/gap-player-matrix.md), [player compatibility](docs/releases.md#player-compatibility), and [validation exceptions](docs/ci-validation.md).

## Development

```sh
git clone https://github.com/darfink/rushls.git
cd rushls
cargo build --release --locked
./target/release/rushls --config rushls.toml
```

The checkout's `rushls.toml` binds every listener to loopback. Run the checks before sending a change:

```sh
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features --locked -- -D warnings
cargo test --workspace --all-features --locked
python3 -m unittest discover -s tools -p 'test_*.py'
```

CI also runs the tagged publishing commands in this README and checks every rendition they produce, and loads every TOML block here through the real configuration loader.
Media, browser, and Apple checks need extra tools; see [CI validation](docs/ci-validation.md) and [architecture](docs/architecture.md).

## License

Rushls uses the [MIT license](LICENSE). Third-party components keep their own licenses.
