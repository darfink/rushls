# Publishing

These recipes publish to the example loopback configuration with open local admission.
Run one publisher at a time per stream name. All shell commands use POSIX syntax.

## Tools

Before running the recipes, check that your installed tools include the codecs and protocols you need (`libx264`, AAC, and `srt` for SRT recipes):

```sh
ffmpeg -version
ffmpeg -protocols
ffmpeg -encoders
gst-launch-1.0 --version
moq --version
```

Builds vary in which protocols they compile in, even at the same version.
For SRT, `srt` must appear in `ffmpeg -protocols`—for example, on macOS Homebrew's `ffmpeg` omits SRT while `ffmpeg-full` includes it.

Start Rushls with the example configuration, which binds every listener to loopback:

```sh
rushls --print-config-example > rushls.toml
rushls --config rushls.toml
```

A stream published as `live/demo` has its multivariant playlist at `http://127.0.0.1:8080/live/demo/index.m3u8`.
When [publisher admission](admission-and-hooks.md) is enabled, your admission service chooses the playback `stream_id`.

## Prepare a test file

Several recipes below reuse a 24-second H.264/AAC file with a 2-second keyframe interval and no B-frames:

<!-- verify: {"id":"fixture"} -->
```sh
ffmpeg -f lavfi -i testsrc2=size=640x360:rate=30 \
  -f lavfi -i sine=frequency=440:sample_rate=48000 \
  -t 24 -c:v libx264 -preset veryfast -pix_fmt yuv420p \
  -g 60 -keyint_min 60 -sc_threshold 0 -bf 0 \
  -c:a aac -b:a 128k -ar 48000 -ac 2 input.mp4
```

To publish an MP4 file over RTMP, use FFmpeg rather than GStreamer—remuxing MP4 to FLV in GStreamer can start with overlapping timestamps.

> [!WARNING]
> `-c copy` sends a file's media unchanged: it does not fix timestamps, convert codecs, or align keyframes.
> Before using `-c copy` on your own file, check its codecs, frame rate, and keyframe interval with `ffprobe`.

## FFmpeg

### RTMP

```sh
ffmpeg -re -i input.mp4 -map 0:v:0 -map 0:a:0 \
  -c copy -f flv rtmp://127.0.0.1:1935/live/ffmpeg
```

Play at `http://127.0.0.1:8080/live/ffmpeg/index.m3u8`.
`-re` paces file input to real time; live capture sources supply their own pacing.

For video-only or audio-only streams, map only the track you want—Rushls does not require both.

### SRT

Quote the URL so your shell leaves `&` alone:

```sh
ffmpeg -re -i input.mp4 -map 0:v:0 -map 0:a:0 -c copy -f mpegts \
  'srt://127.0.0.1:9000?mode=caller&streamid=publish:live/ffmpeg&pkt_size=1316'
```

Play at `http://127.0.0.1:8080/live/ffmpeg/index.m3u8`.
SRT ingest expects MPEG-TS (`-f mpegts`).

Rushls accepts three SRT `streamid` formats:

| Format | Meaning |
| --- | --- |
| `publish:KEY` | One value acts as both resource name and credential |
| `publish:live/camera:KEY` | Explicit resource name and separate credential |
| `#!::u=KEY,r=live/camera,m=publish,t=stream` | Standard SRT access-control format |

URL-encode reserved characters in credentials and resource names, and remember that SRT passphrase encryption (`ingest.srt.passphrase`) is separate from admission credentials.
Note that FFmpeg's SRT `latency` parameter is in microseconds, whereas Rushls's `ingest.srt.latency` uses duration strings such as `"120ms"`.

## GStreamer

Verify the required plugins are installed:

```sh
for element in qtdemux h264parse aacparse flvmux rtmpsink mpegtsmux srtsink; do
  gst-inspect-1.0 "$element" >/dev/null || exit 1
done
```

Distributions split these across GStreamer's Base, Good, Bad, and Ugly plugin packages (and `x264enc`/`avenc_aac` for live encoding typically come from Ugly and Libav). Checking elements with `gst-inspect-1.0` is more reliable than checking package names.

### Synthetic live RTMP source

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

Play at `http://127.0.0.1:8080/live/synthetic/index.m3u8`.
Press Ctrl+C to stop (`-e` sends an orderly end-of-stream).

### SRT from the prepared file

```sh
gst-launch-1.0 -e filesrc location=input.mp4 ! qtdemux name=d \
  d.video_0 ! queue ! h264parse ! mux. \
  d.audio_0 ! queue ! aacparse ! mux. \
  mpegtsmux name=mux alignment=7 ! \
  srtsink uri='srt://127.0.0.1:9000?mode=caller&streamid=publish:live/gstreamer' sync=true
```

`alignment=7` packs seven 188-byte MPEG-TS packets per UDP payload (1,316 bytes), and `sync=true` paces file output in real time.

## Multitrack publishing

One publisher connection supplies every track in an adaptive presentation.
Send all tracks during startup with aligned video keyframes on a shared timeline, and keep decoder configurations steady afterward.
Separate stream names always stay separate streams; Rushls does not merge multiple publishers into one ladder.

### Two video variants over SRT

Encode a 360p and 180p ladder from the 30 fps `input.mp4` fixture:

```sh
ffmpeg -re -i input.mp4 \
  -filter_complex '[0:v]split=2[hi][lo];[lo]scale=320:180[small]' \
  -map '[hi]' -map '[small]' -map 0:a:0 \
  -c:v libx264 -preset veryfast -pix_fmt yuv420p \
  -g 60 -keyint_min 60 -sc_threshold 0 -bf 0 \
  -b:v:0 1200k -b:v:1 400k -c:a aac -b:a 128k \
  -f mpegts 'srt://127.0.0.1:9000?mode=caller&streamid=publish:live/ladder&pkt_size=1316'
```

Play at `http://127.0.0.1:8080/live/ladder/index.m3u8` to see two video variants sharing one audio rendition.
For other frame rates, set `-g` and `-keyint_min` to `2 × fps` to keep a 2-second keyframe interval.

### Enhanced RTMP

The same ladder works over Enhanced RTMP with FFmpeg 9.0 or later (whose FLV muxer writes multitrack Enhanced RTMP):

<!-- verify: {"id":"ladder-rtmp","stream":"ladder","video":2,"audio":1} -->
```sh
ffmpeg -re -i input.mp4 \
  -filter_complex '[0:v]split=2[hi][lo];[lo]scale=320:180[small]' \
  -map '[hi]' -map '[small]' -map 0:a:0 \
  -c:v libx264 -preset veryfast -pix_fmt yuv420p \
  -g 60 -keyint_min 60 -sc_threshold 0 -bf 0 \
  -b:v:0 1200k -b:v:1 400k -c:a aac -b:a 128k \
  -f flv rtmp://127.0.0.1:1935/live/ladder
```

Older FFmpeg builds reject multiple video tracks in FLV. Rushls accepts both `OneTrack` and packed `ManyTracks` Enhanced RTMP packets.

### Alternate audio over SRT

Publish one video track alongside two audio tracks tagged with ISO 639 languages:

<!-- verify: {"id":"alternate-audio","stream":"languages","video":1,"audio":2,"languages":["en","es"]} -->
```sh
ffmpeg -re -i input.mp4 -map 0:v:0 -map 0:a:0 -map 0:a:0 \
  -c copy -metadata:s:a:0 language=eng -metadata:s:a:1 language=spa \
  -f mpegts 'srt://127.0.0.1:9000?mode=caller&streamid=publish:live/languages&pkt_size=1316'
```

Play at `http://127.0.0.1:8080/live/languages/index.m3u8`. (This duplicates the test audio track; swap in a real second audio source in production.)
Rushls turns MPEG-TS ISO 639 descriptors into HLS `LANGUAGE` attributes. MPEG-TS has no per-track title field, so audio tracks receive generic names such as `Audio 2`.

With GStreamer, place `taginject` after each parser and before `mpegtsmux` (two- and three-letter ISO 639 codes both work):

<!-- verify: {"id":"gstreamer-languages","stream":"gst-languages","video":1,"audio":2,"languages":["en","es"]} -->
```sh
gst-launch-1.0 -e filesrc location=input.mp4 ! qtdemux name=d \
  d.video_0 ! queue ! h264parse ! mux. \
  d.audio_0 ! queue ! aacparse ! tee name=audio \
  audio. ! queue ! taginject tags="language-code=en" ! mux. \
  audio. ! queue ! taginject tags="language-code=es" ! mux. \
  mpegtsmux name=mux alignment=7 ! \
  srtsink uri='srt://127.0.0.1:9000?mode=caller&streamid=publish:live/gst-languages' sync=true
```

## OBS Studio

In **Settings → Stream**, choose **Custom**:

- **Server:** `rtmp://127.0.0.1:1935/live`
- **Stream key:** `obs` (or the key issued by your admission service)
- **Output:** H.264 video, AAC audio, and a 2-second keyframe interval

Play at `http://127.0.0.1:8080/live/obs/index.m3u8`. OBS's own multitrack video mode is not covered here.

## Media over QUIC

See the [MoQ guide](moq.md) for catalog rules, captions, and browser publishing.
This example uses `moq-cli` 0.13 (`moq`) against an origin with a trusted TLS certificate:

```toml
[ingest.moq]
listen = "0.0.0.0:4433"

[tls]
cert = "/run/secrets/fullchain.pem"
key = "/run/secrets/private-key.pem"
```

```sh
ffmpeg -re -i input.mp4 -map 0:v:0 -map 0:a:0 -c copy -f mpegts - \
  | moq --connect https://origin.example.com:4433/ \
      --broadcast live/moq --connect-version moq-lite-06 import ts
```

Without admission remapping, playback is at `/live/moq/index.m3u8`. The `https://` URL selects WebTransport over UDP (not the HTTP/HTTPS playback listener).
For local testing with a self-signed certificate, see the [local MoQ test](moq.md#local-publish-test) and [browser publish test](moq.md#browser-publish-test).

## Captions and subtitles

Rushls supports two caption paths:

- **WebVTT subtitle renditions:** Over RTMP, `onCaption` or `onTextData` script messages carrying UTF-8 text (on the RTMP millisecond clock) become a WebVTT subtitle rendition—provided a message arrives during startup so Rushls knows the track exists. Over MoQ, any `utf8` text rendition declared in the catalog becomes a WebVTT rendition from the start.
- **Closed captions (CEA-608/708):** Embedded ATSC CEA-608/708 captions inside H.264 or HEVC SEI payloads stay in the video bitstream and are declared in the multivariant playlist (`TYPE=CLOSED-CAPTIONS`). Rushls does not convert them to WebVTT. Note that a mixed-codec ladder containing a codec Rushls cannot inspect for captions prevents a global caption declaration.

Mapping a subtitle file with `ffmpeg -map` does not emit RTMP `onCaption`/`onTextData` messages. AV1 caption extraction, MPEG-TS subtitle PIDs, and standalone WebVTT/SubRip uploads are not supported.

### Publish captions with gst-captions

The companion [gst-captions](https://github.com/darfink/gst-captions) plugin provides `captionsflvmux`, which injects `onCaption` / `onTextData` messages into FLV before RTMP transmission (as well as optional live transcription and roll-up elements).

Build the model-free muxer (requires GStreamer development headers and `pkg-config`; on macOS with the official GStreamer framework, add `Versions/Current/lib/pkgconfig` to `PKG_CONFIG_PATH`):

```sh
git clone https://github.com/darfink/gst-captions.git
cd gst-captions
git checkout 8fbeda5a33ddc8fbd9a90ba1219bee3e288919ca
cargo build --release --locked --no-default-features --features flvmux
export GST_PLUGIN_PATH="$PWD/target/release${GST_PLUGIN_PATH:+:$GST_PLUGIN_PATH}"
gst-inspect-1.0 captionsflvmux
```

This build needs no speech model. See the [gst-captions build instructions](https://github.com/darfink/gst-captions#build) for other options.

With Rushls running, publish 24 seconds of test video/audio alongside a 2-second subtitle cue:

<!-- verify: {"id":"captions", "stream":"captions", "video":1, "audio":1, "subtitles":1, "text":"Hello from gst-captions"} -->
```sh
cat > captions.srt <<'CAPTIONS'
1
00:00:00,000 --> 00:00:02,000
Hello from gst-captions
CAPTIONS

gst-launch-1.0 -e \
  videotestsrc num-buffers=720 ! video/x-raw,format=I420,width=640,height=360,framerate=30/1 \
  ! x264enc tune=zerolatency key-int-max=60 option-string=scenecut=0 bitrate=1200 ! h264parse ! queue ! fm. \
  audiotestsrc num-buffers=1125 samplesperbuffer=1024 ! audio/x-raw,rate=48000,channels=2 \
  ! avenc_aac ! aacparse ! queue ! fm. \
  flvmux name=fm streamable=true ! captionsflvmux name=cm input-mode=timed prime=true \
  ! rtmp2sink location=rtmp://127.0.0.1:1935/live/captions sync=true \
  filesrc location=captions.srt ! subparse ! text/x-raw,format=utf8 ! queue ! cm.text
```

Play `http://127.0.0.1:8080/live/captions/index.m3u8` and enable subtitles in your player.

A few pipeline details matter here:

- Use `rtmp2sink` rather than the legacy `rtmpsink`, which can fail to deliver codec setup with `captionsflvmux`.
- Keep `streamable=true` so FLV buffers keep their media timestamps.
- Keep `prime=true` so `captionsflvmux` declares the subtitle track during Rushls startup, even if nobody speaks right away.
- Keep cues short and update or clear them promptly: Rushls closes an open script cue when the next message replaces or clears it, and gaps in timestamp coverage on the text branch can backpressure the video/audio branch.

For live speech, feed `cm.text` with `captionstranscriber ! captionsrollup` and `input-mode=replacement` (requires building `gst-captions` with its transcription feature and a speech model).

## Check the result

While a publisher is active, inspect the multivariant playlist:

```sh
curl --fail http://127.0.0.1:8080/live/ladder/index.m3u8
```

Look for `EXT-X-STREAM-INF` entries (video variants) and `EXT-X-MEDIA` entries (audio and subtitle renditions).
Then decode a few seconds with FFmpeg:

```sh
ffmpeg -v error -i 'http://127.0.0.1:8080/live/ladder/index.m3u8' \
  -t 5 -f null -
```

That decodes the default variant; repeat against each child playlist URL (resolved relative to the multivariant playlist) to check every rendition.

## Automated recipe verification

CI runs every recipe in this guide and [README.md](../README.md) marked with a `verify` comment against a fresh Rushls build on every change, checking the advertised renditions, audio languages, subtitle text, and 2 seconds of decoded output per track ([`tools/check-doc-examples.py`](../tools/check-doc-examples.py)).
The OBS and MoQ recipes are not run in CI.
