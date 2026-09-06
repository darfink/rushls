# Native ingest vs avformat — feature parity

Status of what the FFmpeg/avformat path used to do, after the native RTMP +
MPEG-TS adapters and transmux CMAF writer. Delivery, HLS projection, pacing,
and store are unchanged and are not listed here.

Legend: **done** (wired and covered), **extractable** (bytes already in hand,
not copied onto `DiscoveredTrack`), **mapped** (codec/box path exists, no live
or HTTP proof), **gap** (dropped or refused), **untested**.

## Ingest containers

| Feature | Status | Notes |
|---|---|---|
| RTMP / Enhanced RTMP, one video + one audio | **done** | Native `cc-rtmp` → `Packet`. Live `mediastreamvalidator` on H.264 + AAC-LC. |
| SRT MPEG-TS | **done** | `StreamingTsDemux`. Unit fixture is H.264 + ADTS AAC. |
| SRT Matroska / WebM | **gap** | EBML magic refused on purpose. |
| SRT FLV, MP4/fMP4, anything else avformat probed | **gap** | MPEG-TS only. |
| Enhanced RTMP extra A/V tracks | **done** | OneTrack and packed `ManyTracks` become distinct catalog entries keyed by `audio/{id}` / `video/{id}`. Legacy default track stays `audio` / `video`. Identical sequence headers are ignored. Changed codec configurations fail the publish; a new id after freeze is `TrackSetChanged`. HTTP e2e: two OneTrack AAC → two `STREAM-INF` audio variants. |
| MPEG-TS extra audio/video PIDs | **done** | Mapper keeps every mapped PID. Admission default is `tracks = Any`. Apple HLS: H.264 + two AAC-LC PIDs, `mediastreamvalidator` clean. |

## Codecs

| Feature | Status | Notes |
|---|---|---|
| H.264 + AAC-LC, RTMP | **done** | HTTP e2e + Apple validator (cleartext HTTP/1.1 → HTTP/2 warning only). |
| H.264 + AAC, MPEG-TS | **done** | Discovery + CMAF round-trip in-process. The MPEG-TS Apple matrix passes. |
| HEVC | **done** (Apple matrix) | Apple matrix passes for RTMP HEVC + AAC and MPEG-TS HEVC + AAC. |
| AV1 | **done** (decode) / **untested** (live) | RTMP mapping and CMAF writer exist. TS now accepts GStreamer AV1G private PES with its av1C descriptor and an in-band sequence header. Native round-trip and independent FFmpeg decode pass. Other AV1 TS mappings remain unsupported. |
| Opus over RTMP | **done** (in-process) / **untested** (live playback) | A stereo silence packet passes RTMP parsing, normalization, CMAF writing, and demuxing. The test checks 312-sample pre-skip, 48 kHz playback, and separate 44.1 kHz input metadata. |
| Opus over MPEG-TS | **done** (mono/stereo, decode) | Resolves the Opus PMT descriptors and parses PES control headers before shared normalization. Preserves startup and final trim at 48 kHz. Extended channel mappings remain unsupported. |
| AAC-HE / HEv2 frame size | **done** (signaled SBR/PS) | Shared ASC parser handles hierarchical and sync-extension signaling, core/output sample rates, and 960/1024 core frames. Real HE and HEv2 FLV fixtures yield 2048 output samples at 48 kHz and decode through FFmpeg. Implicit SBR without ASC signaling still needs bitstream detection. |
| MPEG-TS AAC-LATM | **gap** | Avformat refused the publisher. Native skips the PID (video-only publish possible). |
| AC-3 / E-AC-3 / MP3 / VVC / VP9 | **gap** | Unmapped; admission would refuse them anyway. |

## Language, title, captions

| Feature | Status | Notes |
|---|---|---|
| MPEG-TS ISO 639 (PMT `ISO_639_language_descriptor`, tag `0x0A`) | **done** | First three-letter code on `TrackSpec.es_info_descriptors` becomes `DiscoveredTrack.language`. Catalog canonicalizes (`eng` → `en`, `spa` → `es`). Covered by `discovers_iso_639_language_on_each_audio_pid`. |
| RTMP `onMetaData` language | **done** | Catalog freeze applies per-track language, then audio/video-specific and generic metadata. Empty, oversized, or control-containing text is ignored. |
| Track `title` | **done** (RTMP) / **gap** (TS) | Uses the same bounded metadata precedence as language. TS service names remain unavailable. |
| HLS `LANGUAGE` on `EXT-X-MEDIA` | **done** | Both native adapters supply source language when available. Catalog canonicalization and HLS projection remain shared. |
| RTMP AMF `onCaption` / `onTextData` | **done** | Drain-before-freeze. HTTP e2e WebVTT rendition. |
| H.264 SEI CEA-608/708 declaration | **done** | Scanner kept; proven on AVCC from native RTMP. |
| HEVC/AV1 in-band captions | **gap** | Detector is H.264-only (same as avformat). |
| Container WebVTT / SubRip / MovText | **gap** | Muxer still converts SubRip; MPEG-TS subtitle PIDs are skipped; nothing ingest feeds WebVTT/SubRip. |
| WebVTT cue id/settings, SubRip position | **gap** | Lived on FFmpeg packet side data. |

## Timing (two different elst cases)

Delayed start and encoder priming are not the same edit list.

| Feature | Status | Notes |
|---|---|---|
| Different first PTS per track (RTMP and MPEG-TS) | **done** | Each track records its first presentation timestamp. Audio adds declared priming to the encoded timestamp. RTMP video includes its composition offset. Mux rebases by a shared `presentation_origin_pts`. A later-starting track gets an empty edit (`media_time = -1`) and a positive chunk `media_start`. Covered by `audio_and_video_keep_their_relative_offset_through_packaging` and `a_genuine_later_video_start_is_exposed_as_positive_media_start`. RTMP discovery asserts video `first_pts = 0`, audio `21`. |
| AAC skip-samples / `initial_padding` | **mapped** (mux) / **gap** (ingest) | Mux writes priming into `elst` when `AudioTiming.initial_padding_samples` or packet `AudioTrim` is set. AAC adapters leave both default. Relevant for **file-originated AAC** (MP4/MKV encoder delay, often 1024 or 2112 for HE). **Live RTMP/TS AAC-LC almost never carries skip-samples**; a later first audio PTS is delayed start, not priming. |
| Opus `pre_skip` | **done** (RTMP and mono/stereo TS) | `OpusHead` bytes 10–11 contain little-endian pre-skip, after the eight-byte signature. The shared parser preserves gain and channel mapping. Normalization emits leading trim once. CMAF writes `dOps` version 0 and an edit that selects past priming. Delayed starts and priming can coexist. TS takes priming from its first PES control header. |
| Declared `video_delay` (B-frame reorder depth) | **done** (H.264/HEVC) | Shared SPS parsers read H.264 VUI reorder limits and HEVC sub-layer ordering. Real B-frame fixtures cover both. Missing declarations retain zero; AV1 delay inference remains a gap. |

## CMAF writer extras avformat got for free

| Feature | Status | Notes |
|---|---|---|
| Init + fragments, no `sidx`, delayed `moov` | **done** | |
| Edit lists: priming, delayed start, composition offset | **done** | Regression tests cover priming with a delayed start and delayed video with a composition offset. Packet-only leading trim also reaches the edit list. Opus final trim shortens the last fragment sample duration. |
| Opus random-access pre-roll | **done** (boxes) / **untested** (live joining) | Init roll descriptions and per-fragment sample groups count enough preceding packets for 80 ms, including variable durations across fragment boundaries. Startup uses a conservative description when history is unavailable. Unit tests check roll distances; live players must still prove retrieval of prior fragments. |
| `colr` / `pasp` / HDR (`mdcv`/`clli`) | **done** (H.264/HEVC startup) | SPS/VUI supplies color and aspect. Static HDR SEI seen before init emission supplies mastering display and content light boxes. Real fixtures check serialized values. AV1 visual metadata and changes after init remain gaps. |

## Live proof (not unit mapping)

| Publish | Result |
|---|---|
| `ffmpeg -re -c copy -f flv` H.264 High@L4 1080p24 GOP 2s, AAC-LC 48 kHz stereo → RTMP | Playable LL-HLS. `mediastreamvalidator -t 30`: HTTP/2 only once `--http-public-url` is set. |
| HEVC / AV1 / Opus live | Apple matrix passes for HEVC RTMP and MPEG-TS. AV1 and Opus have independent file decoding, but no live player proof. |
| Enhanced RTMP second audio/video | HTTP e2e: two OneTrack AAC-LC variants. Apple HLS HTTPS: `rtmp_multitrack_h264_two_aac`. |
| SRT MPEG-TS live | Apple HLS HTTPS: `srt_h264_aac` (one H.264 + one AAC). Dual-audio TS is the in-process MPEG-TS case, not a second SRT matrix row. |
| MPEG-TS extra audio PIDs | Apple HLS HTTPS: `mpegts_h264_two_aac`. |
| File dump without `-re` | Bounded queues apply backpressure after pre-roll. A closed RTMP input drains all discovery packets across batch boundaries. |

## Adapter boundaries and backpressure

`cc-rtmp` exposes `ElementaryUnit` and `ElementaryCodec` through `elementary_units()`.
This API removes FLV framing and preserves decoder configuration bytes.
It does not choose a media container or write CMAF boxes.
Rushls owns codec interpretation, timestamp normalization, and packaging.

RTMP and MPEG-TS share input-limit validation and first-presentation timestamp accounting.
They retain separate discovery state machines because RTMP sequence headers and TS PMT events have different lifecycles.
RTMP uses an asynchronous queue. MPEG-TS uses a blocking worker for the SRT byte reader.
A common worker abstraction would need both execution models without removing either state machine.

RTMP limits queued bytes and caps the queue at 4096 events.
Terminal state remains observable after the queue drains.
Shutdown wakes waiting readers and writers. The last writer drop interrupts the reader.
The source also limits non-media events per fill, so continuous metadata cannot monopolize a worker.

Discovery has byte and wall-time limits, including the RTMP drain before catalog freeze.
An RTMP probe-byte overflow fails discovery instead of consuming and silently dropping the packet that crosses the limit.
A deadline cannot freeze a catalog without timestamps on all discovered A/V tracks.
Pre-roll then consumes the same source under its own limits to establish segment cadence.
The live pacer starts after pre-roll. Queue backpressure does not require a larger pre-roll queue.

The compiled 120 MiB total describes application payload budgets, not maximum process memory.
The native TS demuxer has additional per-PID PES and probe buffers.
Parsed RTMP objects, queue entries, and allocator overhead also consume memory.

The Opus rules come from [RFC 7845](https://www.rfc-editor.org/rfc/rfc7845.html)
and [Opus in ISOBMFF](https://www.opus-codec.org/docs/opus_in_isobmff.html).
The latter distinguishes startup trimming from random-access pre-roll.

## Regression fixtures and validation

The new synthetic fixtures under `tests/apple_hls/fixtures` cover HE/HEv2,
Opus TS, AV1G TS, H.264 aspect/color, and HEVC static HDR.
See [fixture generation notes](../tests/apple_hls/fixtures/native-codecs.md).
The ordinary library suite requires no external decoder for these fixtures.
`independent_decoder_accepts_native_ts_opus_and_av1g` is an ignored test that requires FFmpeg.
Run it explicitly to check decoding and Opus startup alignment against the TS decoder.
FFmpeg currently emits the 648 padded final samples from the fragmented Opus fixture.
The serialized final duration is correct, but this decoder does not apply that end trim.
The regression checks the full audible prefix against the independently decoded source.
The existing eight-test Apple HLS matrix also passes after these changes.
These checks do not replace AV1/Opus live HLS joining or seeking tests.
