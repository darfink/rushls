# Media over QUIC (MoQ)

Rushls ingests Media over QUIC (MoQ) publications in the `hang` format and serves them as HLS.

MoQ ingest is off by default because QUIC requires TLS. Enable it by setting `ingest.moq.listen` alongside `[tls]`.
The listener accepts `moq-lite-06` and `moq-lite-05` over both WebTransport (`h3` ALPN) and raw QUIC (version ALPN directly) on the same UDP port.
Over WebTransport, Rushls picks the newest of the two versions offered by the client; other versions fail negotiation.
`moq-lite-07` is not accepted yet while its wire format is still changing upstream.

One connection carries one publication. Rushls picks the stream resource from the first non-empty source in this order:

1. The WebTransport `CONNECT` URL path
2. The `SETUP` path
3. The first broadcast announcement

For credentials, a WebTransport `token` query parameter is used when present; otherwise the resource name itself acts as the credential.

## Media formats

Rushls reads `catalog.json` and subscribes to every video, audio, and text rendition it declares.
The first catalog fixes the track set for the publication—adding or removing tracks, or changing codecs, containers, or decoder configurations after startup, ends the publication.
Two exceptions are allowed during startup:

- A later catalog may declare a video frame rate for the first time (changing an already-declared rate fails).
- An in-band `OpusHead` may supply a missing Opus description during discovery; Rushls recalculates the first audible timestamp from its encoder delay before locking the stream.

| Input | Support |
| --- | --- |
| LOC | Property header and elementary payload |
| Legacy Hang | Microsecond timestamp prefix and elementary payload |
| H.264 `avc1` | Catalog `description` carries `avcC` |
| H.264 `avc3` | First access unit carries Annex-B SPS and PPS; Rushls builds `avcC` and converts NAL prefixes |
| HEVC / AV1 | Catalog `description` carries decoder configuration |
| AAC | Catalog `description` carries `AudioSpecificConfig` |
| Opus | Catalog `description`, or mono/stereo configuration derived from channel count |
| Text `utf8` | One cue per frame, displayed until the next cue; an empty cue clears it. Served as WebVTT |
| Text `vtt` / `ttml` | Rejected |
| CMAF and unknown containers | Rejected |
| Renditions on another broadcast | Rejected |

A rendition's catalog `label` (hang 0.21 and later) becomes its HLS `NAME`. Because labels are descriptive, a catalog update that changes only a label never ends the publication (changes after startup are ignored).
Audio and video renditions have no language field in the catalog, so they carry no `LANGUAGE`; a text rendition's `lang` becomes its HLS `LANGUAGE`.

### Captions and subtitles

A `hang` catalog `text` section declares caption renditions, where each frame is one cue.
A cue's timestamp is the moment it appears, on the same clock as audio and video—that shared clock is the only synchronization, so a cue stamped when a video frame is captured appears with that frame.
Each cue stays on screen until the next cue replaces it, and an empty cue clears it.

Startup does not wait for a caption cue, since captions can be silent for minutes.
Rushls subscribes from each track's latest group; if a text track already has a cue on screen from before the first video frame, that cue starts where the presentation starts rather than failing the publication.

Cues must arrive before the HLS part covering their timestamp is published. A cue stamped when it is sent travels alongside video from the same moment and arrives on time; a late cue logs `subtitle cue too late` and appears starting from the next unpublished part.
For live transcription, stamp each cue when it is sent rather than with the timestamp of the speech it transcribes—transcripts always trail audio, so speech-stamped cues would consistently arrive late.

### Timing, ordering, and buffering

MoQ carries presentation timestamps without decode timestamps, so Rushls reconstructs decode timestamps during normalization. Reordered H.264 (B-frames) requires frame timing in the SPS.

The reader requests ordered delivery with a 30-second retention budget and consumes one group at a time. Out-of-order groups wait up to 1 second for a missing sequence, up to a 256-group reorder buffer; missing a group past either limit ends the publication.

The internal MoQ cache targets 16 MiB before evicting. Because the underlying transport library buffers an incoming frame before Rushls inspects its size, cache usage is charged to `memory.per_publisher` one frame behind allocation.

For how Rushls chooses segment and part durations—including closed-GOP requirements and `segment.tolerance` for uneven keyframes—see [Segments and parts](configuration.md#segments-and-parts) and [Irregular keyframe intervals](configuration.md#irregular-keyframe-intervals).

## Local publish test

This test requires FFmpeg, `moq-cli` 0.13 (executable `moq`), and a TLS certificate for `localhost`.
Create a configuration file with your certificate paths:

```toml
[ingest]
idle_timeout = "30s"

[ingest.moq]
listen = "127.0.0.1:4443"

[ingest.rtmp]
listen = "127.0.0.1:0"

[ingest.srt]
listen = "127.0.0.1:0"

[tls]
cert = "/absolute/path/cert.pem"
key = "/absolute/path/key.pem"

[http]
listen = "127.0.0.1:18080"
```

RTMP and SRT do not accept `"off"`, so port `0` binds temporary ports for this test.

Start Rushls from the repository root:

```sh
cargo run -p rushls -- --config /absolute/path/moq-test.toml
```

Publish 30 seconds of H.264 video and AAC audio (`moq import ts` converts the MPEG-TS pipe into legacy Hang with Annex-B `avc3` video and AAC audio; Rushls does not receive raw MPEG-TS over MoQ):

```sh
ffmpeg -hide_banner -loglevel error -re \
  -f lavfi -i testsrc2=size=640x360:rate=30 \
  -f lavfi -i sine=frequency=440:sample_rate=48000 \
  -t 30 -c:v libx264 -preset veryfast -g 30 -pix_fmt yuv420p \
  -c:a aac -ar 48000 -ac 2 -f mpegts - \
| moq --connect https://localhost:4443/ \
    --broadcast my-stream --connect-version moq-lite-06 \
    --connect-tls-insecure import ts
```

> [!NOTE]
> `--connect-tls-insecure` is for this local self-signed test only. Production publishers must verify the server certificate.
> To test raw QUIC instead of WebTransport, replace `https://localhost:4443/` with `moqt://localhost:4443/`.

With default 6-second segments, the stream becomes playable after roughly six seconds. Once Rushls logs `playable`, decode three seconds over HLS:

```sh
ffmpeg -hide_banner -loglevel error \
  -i http://127.0.0.1:18080/my-stream/index.m3u8 -t 3 -f null -
```

## Browser publish test

[`tools/browser-moq-publish.html`](../tools/browser-moq-publish.html) publishes your camera, microphone, and typed captions from Chrome over WebTransport.

Chrome does not trust locally installed root certificates for WebTransport, so the page pins the server certificate using `serverCertificateHashes`, fetching the SHA-256 fingerprint from Rushls at `/certificate.sha256` (served only while MoQ ingest is enabled).
Chrome accepts a pinned certificate only if it is ECDSA P-256 and valid for at most 14 days. [`tools/mint-dev-cert.sh`](../tools/mint-dev-cert.sh) generates a 13-day certificate; re-run it whenever it approaches expiry, and Rushls reloads it without a restart. A production origin with a publicly trusted certificate needs no pinning.

From the repository root, create the development certificate and start Rushls:

```sh
./tools/mint-dev-cert.sh
cargo run -- \
  --ingest-moq-listen 127.0.0.1:18080 --ingest-idle-timeout 30s \
  --tls-cert ~/.rushls/dev-tls/cert.pem \
  --tls-key ~/.rushls/dev-tls/key.pem \
  --http-listen 127.0.0.1:18080 \
  --ingest-rtmp-listen 127.0.0.1:0 --ingest-srt-listen 127.0.0.1:0
```

In another terminal, serve the `tools` directory over HTTP:

```sh
python3 -m http.server 18099 --bind 127.0.0.1 --directory tools
```

Open `http://localhost:18099/browser-moq-publish.html` in Chrome and click **Camera + Mic**:

- Leave **Captions (utf8)** checked to publish a subtitle track (it appears in the multivariant playlist as `English`).
- Type a caption and press Enter, or enable **Auto** to send one every two seconds; **Clear** removes the current cue.
- Uncheck **Audio (Opus)** before starting if you want a video-only test (and drop `-map 0:a:0` from the decode command below).

Rushls also accepts the IPv6 loopback address that Chrome may choose for `localhost`. Once Rushls logs `playable`, decode 30 seconds of the stream:

```sh
ffmpeg -hide_banner -xerror \
  -i http://127.0.0.1:18080/web/index.m3u8 -t 30 \
  -map 0:v:0 -map 0:a:0 -fps_mode:v passthrough -enc_time_base:v demux -f null -
```

Passing `-fps_mode:v passthrough -enc_time_base:v demux` preserves variable video timestamps from browser capture.
Because browser capture can produce uneven keyframe intervals, configure [`hls.segment` tolerance](configuration.md#irregular-keyframe-intervals) if your browser misses boundaries (tolerance defaults to `0s`).

## Browser ladder test

To test a multi-rendition WebCodecs ladder (1080p, 720p, 360p, and stereo audio under `ladder`), reuse the certificate and HTTP server from above, and add 2-second segments and 500 ms parts to the Rushls command:

```sh
--hls-segment 2s --hls-part 500ms
```

Open `http://localhost:18099/browser-ladder-bench.html` in Chrome, keep audio and publishing enabled, and click **Synthetic 1080p** (the test pattern includes a quiet 440 Hz tone).
You can click **Freeze source** to confirm the browser pacer keeps emitting frames when the source picture is static.

From the repository root, verify and decode all four HLS tracks (requires `ffmpeg` and `ffprobe`):

```sh
python3 tools/check-browser-ladder.py --seconds 30
```

The script checks codec metadata and decodes 30 seconds of output from every rendition, allowing up to 4 extra seconds for differing live join points.

The test page warms all video encoders and waits for Rushls to subscribe to every track before starting the media clock. If one encoder falls behind, all renditions skip the same input frame so keyframes stay aligned (and any pending keyframe request carries over to the next accepted frame). The hardware toggle requests `prefer-hardware`, though WebCodecs does not report which backend it actually chose.
