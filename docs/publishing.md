# Publishing

These recipes target the bundled loopback configuration and unauthenticated local publishing.
Use one publisher at a time for each stream name.
The examples use POSIX shell syntax.

## Tested tools

The recipe validation uses FFmpeg 9.0.1, GStreamer 1.28.6, and `moq-cli 0.10.0` on macOS.
FFmpeg must include `libx264`, the AAC encoder, and SRT support for the SRT commands.
Different FFmpeg distributions include different transports, even with the same version number.

```sh
ffmpeg -version
ffmpeg -protocols
ffmpeg -encoders
gst-launch-1.0 --version
moq --version
```

The SRT protocol must appear in FFmpeg's protocol list.
On the validation host, Homebrew's `ffmpeg-full` provides SRT; the default `ffmpeg` binary does not.
Select the SRT-enabled executable in your `PATH` before using the recipes.

Start Rushls with the example configuration, which binds every listener to loopback:

```sh
rushls --print-config-example > rushls.toml
rushls --config rushls.toml
```

A stream named `live/demo` has the master playlist `http://127.0.0.1:8080/live/demo/index.m3u8`.
An admission service can replace that name with its returned `stream_id`.

## Prepare a test file

Generate 24 seconds of H.264/AAC with a two-second GOP and no B-frames:

<!-- verify: {"id":"fixture"} -->
```sh
ffmpeg -f lavfi -i testsrc2=size=640x360:rate=30 \
  -f lavfi -i sine=frequency=440:sample_rate=48000 \
  -t 24 -c:v libx264 -preset veryfast -pix_fmt yuv420p \
  -g 60 -keyint_min 60 -sc_threshold 0 -bf 0 \
  -c:a aac -b:a 128k -ar 48000 -ac 2 input.mp4
```

The later file recipes use this fixture.
A direct GStreamer MP4-to-FLV remux failed on this host with overlapping startup timestamps, including with this fixture.
Use the synthetic RTMP pipeline below or FFmpeg for RTMP file publishing.
This is a conversion-path finding, not a blanket restriction on reordered video in Rushls.

For an existing file, inspect its codecs, frame rate, and keyframes before using stream copy.
`-c copy` does not repair timestamps, change codecs, or align keyframes.

## FFmpeg

### RTMP

```sh
ffmpeg -re -i input.mp4 -map 0:v:0 -map 0:a:0 \
  -c copy -f flv rtmp://127.0.0.1:1935/live/ffmpeg
```

Playback: `http://127.0.0.1:8080/live/ffmpeg/index.m3u8`.
`-re` paces file input. Live capture devices normally supply their own timing.

For video-only or audio-only publishing, map only the required track.
Rushls accepts supported media without requiring both kinds.

### SRT

```sh
ffmpeg -re -i input.mp4 -map 0:v:0 -map 0:a:0 -c copy -f mpegts \
  'srt://127.0.0.1:9000?mode=caller&streamid=publish:live/ffmpeg&pkt_size=1316'
```

Playback: `http://127.0.0.1:8080/live/ffmpeg/index.m3u8`.
The payload is MPEG-TS. Renaming a Matroska or MP4 output does not change its container.

Rushls accepts these stream-ID forms:

| Form | Meaning |
| --- | --- |
| `publish:KEY` | One value supplies the resource and credential |
| `publish:live/camera:KEY` | Explicit resource and separate credential |
| `#!::u=KEY,r=live/camera,m=publish,t=stream` | SRT access-control convention for compatible clients |

URL-encode reserved characters in credentials and resource values.
Keep the whole URL quoted in shell commands.
SRT passphrase encryption is independent of these admission credentials.
FFmpeg URL `latency` uses microseconds; Rushls TOML uses duration strings such as `"120ms"`.

## GStreamer

Check the required elements:

```sh
for element in qtdemux h264parse aacparse flvmux rtmpsink mpegtsmux srtsink; do
  gst-inspect-1.0 "$element" >/dev/null || exit 1
done
```

Distribution packages usually divide these elements across the Base, Good, Bad, and Ugly plugin sets.
Synthetic encoding also requires `x264enc` and `avenc_aac`, often supplied by Ugly and Libav packages.
Use element availability rather than package names as the final check.

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

Playback: `http://127.0.0.1:8080/live/synthetic/index.m3u8`.
Stop the pipeline with Ctrl+C. `-e` requests end-of-stream handling on interruption.

### SRT from the prepared file

```sh
gst-launch-1.0 -e filesrc location=input.mp4 ! qtdemux name=d \
  d.video_0 ! queue ! h264parse ! mux. \
  d.audio_0 ! queue ! aacparse ! mux. \
  mpegtsmux name=mux alignment=7 ! \
  srtsink uri='srt://127.0.0.1:9000?mode=caller&streamid=publish:live/gstreamer' sync=true
```

`alignment=7` groups seven 188-byte transport packets per output buffer.
`sync=true` preserves file playback pacing at the sink.

## Multitrack publishing

One connection supplies all tracks for one publication.
The publisher must send the full track set during discovery, with stable decoder configuration afterward.
Use aligned keyframes for video variants and a shared media timeline.
Different stream names do not combine into one ladder.

### Two video variants over SRT

This recipe encodes two variants from the prepared 30 fps, 16:9 file:

```sh
ffmpeg -re -i input.mp4 \
  -filter_complex '[0:v]split=2[hi][lo];[lo]scale=320:180[small]' \
  -map '[hi]' -map '[small]' -map 0:a:0 \
  -c:v libx264 -preset veryfast -pix_fmt yuv420p \
  -g 60 -keyint_min 60 -sc_threshold 0 -bf 0 \
  -b:v:0 1200k -b:v:1 400k -c:a aac -b:a 128k \
  -f mpegts 'srt://127.0.0.1:9000?mode=caller&streamid=publish:live/ladder&pkt_size=1316'
```

Playback: `http://127.0.0.1:8080/live/ladder/index.m3u8`.
The expected output has two video variants and a shared audio rendition.
For another frame rate, adjust the GOP length to preserve the intended keyframe interval.

### Enhanced RTMP

The same encoded ladder can use Enhanced RTMP with FFmpeg 9.0.1:

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

Older FLV muxers can reject multiple tracks or lack the required Enhanced RTMP messages.
The tested version is not a claim about the earliest compatible FFmpeg release.
Rushls supports OneTrack and packed ManyTracks messages.
The publisher controls their wire representation; these commands do not use a synthetic Rushls test publisher.

### Alternate audio over SRT

Publish one video track and two audio tracks with language metadata:

<!-- verify: {"id":"alternate-audio","stream":"languages","video":1,"audio":2,"languages":["en","es"]} -->
```sh
ffmpeg -re -i input.mp4 -map 0:v:0 -map 0:a:0 -map 0:a:0 \
  -c copy -metadata:s:a:0 language=eng -metadata:s:a:1 language=spa \
  -f mpegts 'srt://127.0.0.1:9000?mode=caller&streamid=publish:live/languages&pkt_size=1316'
```

Playback: `http://127.0.0.1:8080/live/languages/index.m3u8`.
Both audio tracks contain the same test audio. Replace the second mapping with a real alternate-language source in production.
Rushls maps MPEG-TS ISO 639 language descriptors into HLS language metadata.
Track titles have different transport coverage; language tags do not imply arbitrary title metadata support.

With GStreamer, `taginject` sets each track's language, and `mpegtsmux` writes it as the ISO 639 descriptor.
Both two-letter and three-letter codes work. This publishes the prepared file's audio twice, as English and Spanish:

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

Place `taginject` after the parser and before `mpegtsmux`, one per track.
MPEG-TS has no per-track name, so a `title` tag is dropped and the rendition gets a generic name such as `Audio 2`.

## OBS Studio

Use the Custom service with server `rtmp://127.0.0.1:1935/live` and stream key `obs`.
Select H.264 video, AAC audio, and a two-second keyframe interval.
Playback uses `http://127.0.0.1:8080/live/obs/index.m3u8`.

With admission enabled, the application supplies the stream key and selects the resulting playback ID.
This documents conventional RTMP field mapping. This overhaul does not certify an OBS version or its multitrack mode.

## Media over QUIC

Use the [MoQ guide](moq.md) for the exact `moq-lite-05` contract.
The tested client is `moq-cli 0.10.0`, whose executable is `moq`.

For a public deployment, configure a certificate that covers the origin hostname and is trusted by the client:

```toml
[ingest.moq]
listen = "0.0.0.0:4433"

[tls]
cert = "/run/secrets/fullchain.pem"
key = "/run/secrets/private-key.pem"
```

```sh
ffmpeg -re -i input.mp4 -map 0:v:0 -map 0:a:0 -c copy -f mpegts - \
  | moq --client-connect https://origin.example.com:4433/ \
      --broadcast live/moq --client-version moq-lite-05 import ts
```

The corresponding HTTP playback path is `/live/moq/index.m3u8` when admission does not remap the stream.
The `https` URL selects WebTransport. The transport uses UDP, not the ordinary HTTP listener.

For an isolated local test, create a disposable certificate and configure its paths under `[tls]`.
The existing [local MoQ recipe](moq.md#local-publish-test) uses `--client-tls-disable-verify` for that test only.
Production publishers must verify certificates.
MoQ browser publishing has a manual Chrome procedure in the [MoQ guide](moq.md#browser-publish-test); it does not run in CI.

## Captions and subtitles

RTMP script messages named `onCaption` or `onTextData` can supply UTF-8 text.
They use the RTMP millisecond timeline and must appear during discovery to create a text track.
Messages after discovery only contribute if that track already exists.
Rushls packages these cues as a WebVTT subtitle rendition.

The publisher must implement these messages. Mapping a subtitle file with `ffmpeg -map` does not guarantee this wire format.
The validation harness injects messages explicitly; it is not a generic subtitle publisher CLI.

Embedded ATSC CEA-608/708 captions in H.264 or HEVC remain in their video access units.
Rushls inspects supported SEI payloads for HLS caption declarations.
It does not turn those embedded captions into a separate WebVTT track.
A mixed ladder with an unverifiable codec can prevent a global caption declaration.
AV1 caption extraction, MPEG-TS subtitle PIDs, and standalone WebVTT/SubRip ingestion remain unsupported.

### Publish captions with gst-captions

[gst-captions](https://github.com/darfink/gst-captions) provides `captionsflvmux`, which inserts `onCaption` or `onTextData` into FLV before RTMP transmission.
It also provides optional transcription and roll-up elements.
Rushls receives the caption messages and creates an HLS WebVTT rendition.

Build the model-free muxer from the revision used in Rushls CI:

```sh
git clone https://github.com/darfink/gst-captions.git
cd gst-captions
git checkout 8fbeda5a33ddc8fbd9a90ba1219bee3e288919ca
cargo build --release --locked --no-default-features --features flvmux
export GST_PLUGIN_PATH="$PWD/target/release${GST_PLUGIN_PATH:+:$GST_PLUGIN_PATH}"
gst-inspect-1.0 captionsflvmux
```

The build needs GStreamer development headers and `pkg-config`.
For the official macOS GStreamer framework, first set `PKG_CONFIG_PATH` to its `Versions/Current/lib/pkgconfig` directory.
The model-free build needs no speech model or transcription backend.
See the upstream [build instructions](https://github.com/darfink/gst-captions#build) for other installation methods.

With Rushls running, publish 24 seconds of generated video/audio and a two-second caption:

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

Playback: `http://127.0.0.1:8080/live/captions/index.m3u8`.
Select the subtitle rendition in your player. The master playlist advertises `TYPE=SUBTITLES`.
This is publisher-side conversion of an SRT subtitle file into RTMP messages, not a standalone subtitle upload endpoint in Rushls.

Use `rtmp2sink` for this pipeline; the legacy `rtmpsink` failed to deliver codec setup correctly in our test.
Keep `streamable=true` so FLV buffers retain media timestamps.
Keep `prime=true` so the muxer declares captions during Rushls discovery, even before the first spoken word.
Without that early declaration, late captions cannot add a subtitle track to an already frozen publication.
The text branch needs timestamped coverage; missing coverage can backpressure the media branch.
Rushls resolves a script cue when the next message replaces or clears it.
Use short cues with prompt updates rather than holding one caption open for the whole broadcast.

For live speech, upstream's `captionstranscriber ! captionsrollup` can feed `cm.text` with `input-mode=replacement`.
That workflow requires a model and the transcription feature. The CI sample uses deterministic text and does not measure transcription quality.

## Check the result

Fetch the master playlist while the publisher is active:

```sh
curl --fail http://127.0.0.1:8080/live/ladder/index.m3u8
```

Inspect `EXT-X-STREAM-INF` for variants and `EXT-X-MEDIA` for audio and subtitle renditions.
Resolve relative playlist URLs against the master URL.
Decode each advertised rendition, rather than only the default variant:

```sh
ffmpeg -v error -i 'http://127.0.0.1:8080/live/ladder/index.m3u8' \
  -t 5 -f null -
```

This command checks the player's default selection. Repeat with each child playlist to check all renditions.
Decoder success is separate from browser playback, switching, seek behavior, and perceptual quality.

## Recipe verification

On 2026-09-24, local Rushls tests produced these results with the versions listed at the start of this guide:

| Recipe | Observed HLS output | Decode check |
| --- | --- | --- |
| FFmpeg RTMP file | One video variant, one audio rendition | Both passed |
| FFmpeg SRT file | One video variant, one audio rendition | Both passed |
| FFmpeg two-size ladder, Enhanced RTMP | 640×360 and 320×180 variants, one audio rendition | All three passed |
| FFmpeg two-size ladder, SRT | 640×360 and 320×180 variants, one audio rendition | All three passed |
| FFmpeg alternate audio, SRT | One video variant, `en` and `es` audio renditions | All three passed |
| GStreamer live RTMP | One video variant, one audio rendition | Both passed |
| GStreamer SRT file | One video variant, one audio rendition | Both passed |
| Native MoQ file import | One video variant, one audio rendition | Both passed |

Each check decoded two seconds from every advertised audio/video child playlist while publishing.
Local tests substituted free loopback ports and unique stream names to run without listener conflicts.
MoQ used a disposable self-signed certificate with client verification disabled only for that local test.
These checks establish publication, track discovery, playlist output, and initial decoding; they do not certify long-duration playback or every player.
OBS instructions describe field mapping and were not exercised in an OBS session.

Local logs, master playlists, and JSON results reside under `target/readme-validation/` and are not part of a clone.
The failed GStreamer MP4-to-FLV remux is excluded from the recommended recipes.
