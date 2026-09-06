# Native codec regression fixtures

These short files contain generated test patterns and tones, with no external media.
The library tests read them directly; generation tools are not test dependencies.

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

For AV1, encode a raw OBU file with `-c:v libsvtav1 -g 2 -f obu`.
Remux it with this GStreamer pipeline:

```sh
gst-launch-1.0 filesrc location=av1.obu ! av1parse ! \
  mpegtsmux enable-custom-mappings=true ! filesink location=av1.ts
```

Both video encoders require explicit color parameters:
`colorprim=bt2020:transfer=smpte2084:colormatrix=bt2020nc`.
Use `-vf setsar=4/3 -g 2 -bf 1`.
For x265, append these parameters:

```text
master-display=G(13250,34500)B(7500,3000)R(34000,16000)WP(15635,16450)L(10000000,50):max-cll=1000,400
```

Run the independent decoder regression from the workspace root:

```sh
cargo test -p rushls --lib independent_decoder_accepts -- --ignored
```
