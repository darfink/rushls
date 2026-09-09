# Apple HLS fixtures

These files contain generated test patterns and tones, with no external media.
The library tests and the `apple_hls` integration suite read them directly;
generation tools are not test dependencies.

Two groups live here. The short ones below are unit-test inputs for codec
configuration parsing, a few hundred milliseconds each. The longer ones in the
second table are conformance inputs: about eight seconds, which is four segments
at the two-second cadence the integration suite runs, because Apple's validator
needs several completed segments before it will say anything useful.

## Short codec-configuration fixtures

| File | Content and purpose |
|---|---|
| `opus.ts` | 400 ms, 997 Hz tone, stereo, 48 kHz, libopus. Tests PMT discovery, PES framing, and 312-sample startup trim. |
| `av1.ts` | 160×96 test pattern, 10 fps, libsvtav1. GStreamer wraps the OBU stream in its AV1G TS mapping. |
| `he_aac.flv` | 400 ms stereo tone, Apple AudioToolbox HE-AAC, 48 kHz output, 48 kb/s. |
| `hev2_aac.flv` | The same tone with AudioToolbox HEv2, 32 kb/s. |
| `h264_colour.ts` | 160×96 test pattern, 10 fps, SAR 4:3, one B-frame, BT.2020/PQ signaling. |
| `hevc_hdr.ts` | The same video with HEVC mastering-display and content-light SEI. |

Generation uses FFmpeg with libopus, libsvtav1, libx264, libx265, and macOS `aac_at`.
The video duration is 500 ms, and the GOP length is two frames.
For HE-AAC, encode an M4A with `-c:a aac_at -profile:a 4 -b:a 48000`.
For HEv2, use `-profile:a 28 -b:a 32000`.
Then remux each file with `-c:a copy -f flv`.
The preserved ASC includes backward-compatible SBR/PS sync extensions.

For AV1, encode an IVF file with `-c:v libsvtav1 -g 2 -f ivf`.
IVF preserves timestamps. A raw OBU stream can produce a TS file with identical timestamps on every packet.
Remux IVF with this GStreamer pipeline:

```sh
gst-launch-1.0 filesrc location=av1.ivf ! ivfparse ! av1parse ! \
  mpegtsmux enable-custom-mappings=true ! filesink location=av1.ts
```

Both video encoders require explicit color parameters:
`colorprim=bt2020:transfer=smpte2084:colormatrix=bt2020nc`.
Use `-vf setsar=4/3 -g 2 -bf 1`.
For x265, append these parameters:

```text
master-display=G(13250,34500)B(7500,3000)R(34000,16000)WP(15635,16450)L(10000000,50):max-cll=1000,400
```

## Conformance fixtures

Regenerate every file in this table with `tools/make-apple-hls-fixtures.sh`,
which documents the reasoning for each alongside its FFmpeg invocation. Each one
makes exactly one property unusual; everything else about it is ordinary, so a
failure has one candidate cause.

| File | The property it makes unusual |
|---|---|
| `h264_bpyramid_aac.flv` | Eight consecutive B-frames, normal pyramid. Decode order runs seven frame periods behind presentation order. |
| `h264_he_aac.flv` | HE-AAC. The decoded rate is twice the encoded frame rate, and the codec string is `mp4a.40.5`. |
| `h264_2398_aac.ts` | 24000/1001 from a 90 kHz clock: no frame duration is a whole number of output ticks. |
| `h264_ladder3_aac.ts` | Three video renditions with aligned key frames, plus one shared audio track. |
| `hevc_hdr10_aac.ts` | BT.2020 primaries, PQ transfer, mastering-display and content-light SEI. |
| `h264_multilang_aac.ts` | Two audio tracks with distinct ISO 639 codes in the PMT. |
| `h264_aac_441_mono.flv` | 44.1 kHz mono: no common period with a 30 fps video timescale. |
| `h264_aac_51.flv` | 5.1 audio, which a rendition must advertise as `CHANNELS="6"`. |
| `h264_longgop_aac.ts` | A five-second key frame interval, longer than the usual segment target. |
| `h264_anamorphic_aac.ts` | SAR 4:3, so encoded and displayed geometry differ. |
| `h264_opus.ts` | Opus beside H.264 — a codec this origin accepts and Apple's HLS profile does not list. |
| `av1_long.ts` | AV1 with no audio, through GStreamer's AV1G MPEG-TS mapping. |

Run the independent decoder regression from the workspace root:

```sh
cargo test -p rushls --lib independent_decoder_accepts -- --ignored
```
