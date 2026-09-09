# MOQ ingestion

The origin accepts `moq-lite-05` over WebTransport and raw QUIC on the same UDP port.
WebTransport uses `h3` ALPN and selects `moq-lite-05` in the CONNECT response.
Raw QUIC uses `moq-lite-05` ALPN directly. Other protocol versions fail negotiation.

One connection carries one publication. The CONNECT URL supplies the resource when its path is not empty.
Otherwise, the SETUP path supplies the resource. If both paths are empty, the first broadcast announcement supplies the resource.
A WebTransport `token` query parameter supplies the credential. Without that parameter, the resource name supplies the credential.

## Media formats

The origin reads `catalog.json` and subscribes to each audio and video rendition.
The first catalog fixes the track set. Track and container changes fail ingestion.
During discovery, the origin accepts an OpusHead that replaces an omitted Opus description.
It recalculates the first audible timestamp from the encoder delay before discovery ends.
After discovery, decoder configuration changes fail ingestion.

| Input | Support |
| --- | --- |
| LOC | Property header and elementary payload |
| Legacy Hang | Microsecond timestamp prefix and elementary payload |
| H.264 `avc1` | Catalog `description` contains avcC |
| H.264 `avc3` | First access unit contains Annex-B SPS and PPS. The origin builds avcC and converts NAL prefixes. |
| HEVC / AV1 | Catalog `description` contains decoder configuration |
| AAC | Catalog `description` contains AudioSpecificConfig |
| Opus | Catalog description, or mono/stereo configuration from channel count |
| CMAF and unknown containers | Rejected |
| Renditions on another broadcast | Rejected |

MOQ supplies presentation timestamps, without decode timestamps.
The shared normalizer reconstructs decode timestamps. Reordered H.264 requires frame timing from the SPS.

The reader requests ordered delivery and a 30-second retention budget.
It consumes the current group before the next group. Out-of-order groups wait up to one second for a missing sequence.
The reorder buffer holds at most 256 groups. A missing group beyond either limit fails ingestion.

Discovery limits cover subscriptions, catalog bytes, and media probing.
Packet limits apply to returned samples. The MOQ cache has a 16 MiB eviction target.
That target is not a hard allocation limit. The dependency buffers incoming frames before the adapter checks their payload size.

## Local publish test

This test requires FFmpeg, `moq-cli 0.10.0`, and a certificate for `localhost`.
Replace the certificate paths in this configuration:

```toml
[moq]
listen = "127.0.0.1:4443"
timeout = "30s"
certificate = "/absolute/path/cert.pem"
key = "/absolute/path/key.pem"

[rtmp]
listen = "127.0.0.1:0"

[srt]
listen = "127.0.0.1:0"

[http]
listen = "127.0.0.1:18080"
```

RTMP and SRT require socket addresses. Port zero assigns temporary ports. Their `listen` fields do not accept `"off"`.

Start the origin from the repository root:

```sh
cargo run -p rushls -- --config /absolute/path/moq-test.toml
```

Publish H.264 video and AAC audio:

```sh
ffmpeg -hide_banner -loglevel error -re \
  -f lavfi -i testsrc2=size=640x360:rate=30 \
  -f lavfi -i sine=frequency=440:sample_rate=48000 \
  -t 30 -c:v libx264 -preset veryfast -g 30 -pix_fmt yuv420p \
  -c:a aac -ar 48000 -ac 2 -f mpegts - \
| moq --client-connect https://localhost:4443/ \
    --broadcast my-stream --client-version moq-lite-05 \
    --client-tls-disable-verify import ts
```

The verification flag is for this local certificate test. Production clients must verify the server certificate.
The second FFmpeg input supplies audio. An audio codec option alone does not create an audio stream.

After the origin reports `playable`, decode three seconds through HLS:

```sh
ffmpeg -hide_banner -loglevel error \
  -i http://127.0.0.1:18080/my-stream/index.m3u8 -t 3 -f null -
```

For raw QUIC, replace `https://localhost:4443/` with `moqt://localhost:4443/` in the publish command.
The default segmentation settings need approximately six seconds of media before the stream becomes playable.

The client imports TS into legacy Hang with Annex-B `avc3` video and AAC audio.
The origin does not receive MPEG-TS bytes over MOQ.


## Browser publish test

From `.`, create a development certificate and start the origin:

```sh
./tools/mint-dev-cert.sh
cargo run -- \
  --moq-listen 127.0.0.1:18080 --moq-timeout 30s \
  --moq-certificate ~/.rushls/dev-tls/cert.pem \
  --moq-key ~/.rushls/dev-tls/key.pem \
  --http-listen 127.0.0.1:18080 \
  --rtmp-listen 127.0.0.1:0 --srt-listen 127.0.0.1:0
```

In another terminal, serve the test page from `tools`:

```sh
python3 -m http.server 18099 --bind 127.0.0.1
```

Open `http://localhost:18099/browser-moq-publish.html` in Chrome.
Click **Camera + Mic**.
The page fetches the certificate fingerprint from the HTTP listener and pins the WebTransport certificate.
The origin also accepts the IPv6 loopback address that Chrome can select for `localhost`.

After the origin reports `playable`, decode 30 seconds of video and audio:

```sh
ffmpeg -hide_banner -xerror \
  -i http://127.0.0.1:18080/web/index.m3u8 -t 30 \
  -map 0:v:0 -map 0:a:0 -fps_mode:v passthrough -enc_time_base:v demux -f null -
```

The output timebase preserves variable video timestamps during this decode test.
Before capture starts, clear **Audio (Opus)** for a video-only test.
For that test, remove `-map 0:a:0` from the decode command.

Browser capture does not guarantee constant frame durations or exact keyframe intervals.
For variable frame durations, the origin closes each regular part after it reaches 85% of the advertised part target.
Pre-roll replays observed sample durations through the same part cutter used during publication.
Later media must still satisfy the HLS duration limits.

Late-keyframe allowance defaults to zero. Configure an explicit allowance for publishers that need it.
The frozen segment ceiling includes that allowance. All video renditions must supply matching random-access timestamps.
Actual decoder reconfiguration, including changed SPS/PPS, still requires a new publication.


## Browser ladder test

Use the certificate and HTTP server commands above. Add these options to the origin command for two-second segments:

```sh
--hls-segment-target 2s --hls-part-target 500ms
```

Open `http://localhost:18099/browser-ladder-bench.html` in Chrome and click **Synthetic 1080p**.
Keep audio and publishing enabled. The page publishes 1080p, 720p, 360p, and stereo audio under `ladder`.
The synthetic source includes a quiet 440 Hz tone.
Click **Freeze source** to verify that the video pacer continues to produce frames for a static source.

From `.`, verify and decode all four HLS tracks:

```sh
python3 tools/check-browser-ladder.py --seconds 30
```

The checker requires FFmpeg and FFprobe. It checks codec metadata, decodes every rendition, and requires 30 seconds of output per track.
It allows four extra seconds for different live rendition join points.

The publisher warms all video encoders before it starts the media clock.
It also waits for all origin subscriptions. If one encoder falls behind, all renditions skip the same input frame.
A skipped keyframe request remains pending for the next accepted frame.
The hardware option requests `prefer-hardware`; WebCodecs does not expose which encoder backend it selects.


## Adaptive segmentation

The configuration uses target and maximum objects. These are the defaults:

```toml
[hls]
segment = { target = "6s", max = "2x", jitter = "0s" }
part = { target = "1s", max = "2x" }
```

The corresponding CLI flags include `--hls-segment-target 2s` and `--hls-segment-max 3s`.
Maxima accept absolute durations or multipliers of their configured targets.
Equal target and maximum values forbid growth during admission.

Pre-roll tries the latest common video boundary at or before the segment target.
If the complete contract fails, it tries earlier boundaries before it extends the search beyond the target.
Audio boundaries use whole encoded samples. Audio-only input can select an earlier boundary to keep rounding within the maximum.
Admission selects a larger part ceiling only if the preferred ceiling fails and the configured maximum permits growth.
Selected ceilings remain fixed throughout the publication.

The part writer retains a bounded window before it commits a cut.
It can repair nearby cuts without changing published parts or waiting for a whole segment.
Some input sequences still require more lookahead than the window permits and terminate the publication.

Both CFR and VFR use duration-based part cutting. A regular part normally reaches 85% of its ceiling.
Independent parts and segment tails can be shorter. No part can exceed its ceiling.
The origin preserves encoded payloads and normalizes timing. It does not create replacement frames or transcode input.

The coordinator selects matching random-access points across video renditions.
`segment.jitter` defines a symmetric window around each planned boundary.
Its multiplier form resolves against `segment.target`.
The planned timeline never resets after an early or late cut. The actual segment must also fit the frozen ceiling.
The advertised ceiling includes both allowances, plus the required timestamp quantization.
Thus, an early cut followed by a late cut can use the full combined budget.
An observed long GOP during pre-roll does not grant extra late allowance during publication.

The coordinator bounds retained samples and bytes with the publication's pre-roll limits.
The existing stall timeout bounds wall-clock waiting. A missing required track fails the publication; topology does not change automatically.
Valid published media remains available after failure.

The `segmentation contract` event reports desired durations, selected durations, maxima, and jitter.
Failure warnings identify the cause and applicable bounds. Metrics separately count part, boundary, and coordinator-capacity failures.
Retention multiples and hold-back use the selected contract. Incompatible fixed values reject publication before it becomes visible.

Active codec and topology changes remain unsupported. A replacement publication must satisfy the existing playlist contract or receive new rendition playlists.

Admission runs the live coordinator and CMAF timing logic without serializing CMAF fragments.
The factory only constructs the admitted publication. It does not run a hidden validation pass.

Fractional GOP periods stay on an anchored rational grid. Each boundary is rounded separately.
The planner preserves the source timestamp precision through normalization. It does not add a full frame of video slack.
Fixed hold-back and retention settings must cover the maximum admission targets at startup.
Relative retention uses the selected shared target for both expiry and reported retention depth.

AVC segments require IDR frames. Configure the encoder for closed GOPs.
A transport keyframe flag or recovery-point SEI does not establish independent decoding.
Unsupported AVC random-access pictures produce a specific error with this configuration guidance.
