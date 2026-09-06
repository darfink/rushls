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
| Enhanced RTMP extra A/V tracks | **done** | OneTrack and packed `ManyTracks` become distinct catalog entries keyed by `audio/{id}` / `video/{id}`. Legacy default track stays `audio` / `video`. Repeat sequence headers are ignored; a new id after freeze is `TrackSetChanged`. HTTP e2e: two OneTrack AAC → two `STREAM-INF` audio variants. |
| MPEG-TS extra audio/video PIDs | **done** | Mapper keeps every mapped PID. Admission default is `tracks = Any`. Apple HLS: H.264 + two AAC-LC PIDs, `mediastreamvalidator` clean. |

## Codecs

| Feature | Status | Notes |
|---|---|---|
| H.264 + AAC-LC, RTMP | **done** | HTTP e2e + Apple validator (cleartext HTTP/1.1 → HTTP/2 warning only). |
| H.264 + AAC, MPEG-TS | **done** | Discovery + CMAF round-trip in-process. No live SRT validator run. |
| HEVC, AV1, Opus | **mapped** | RTMP FourCC, MPEG-TS `CodecConfig`, and CMAF `hvcC`/`av1C`/`dOps` writers exist. No ingest unit with a real bitstream, no HTTP e2e, no live publish. `cc-rtmp` tests parse enhanced tags; they do not round-trip coded frames through rushls. |
| AAC-HE / HEv2 frame size | **gap** | RTMP hardcodes AAC `frame_size = 1024` (LC). HE is 2048. |
| MPEG-TS AAC-LATM | **gap** | Avformat refused the publisher. Native skips the PID (video-only publish possible). |
| AC-3 / E-AC-3 / MP3 / VVC / VP9 | **gap** | Unmapped; admission would refuse them anyway. |

## Language, title, captions

| Feature | Status | Notes |
|---|---|---|
| MPEG-TS ISO 639 (PMT `ISO_639_language_descriptor`, tag `0x0A`) | **done** | First three-letter code on `TrackSpec.es_info_descriptors` becomes `DiscoveredTrack.language`. Catalog canonicalizes (`eng` → `en`, `spa` → `es`). Covered by `discovers_iso_639_language_on_each_audio_pid`. |
| RTMP `onMetaData` language | **extractable** | `ParsedMetadata.properties` keeps every AMF key. Common names: `language`, `audiolanguage`, `videolanguage`. Enhanced per-track maps keep their own `properties`. OBS usually sends none. |
| Track `title` | **extractable** (RTMP) / **gap** (TS) | RTMP: `onMetaData.title` if present (rare). MPEG-TS has no per-ES title; SDT service name is not on `TrackSpec`. Matroska tags were the old `title` source. |
| HLS `LANGUAGE` on `EXT-X-MEDIA` | **done** (pipeline) | Projector emits it when `DiscoveredTrack.language` is set. MPEG-TS fills it from PMT; RTMP adapters still leave it unset. Apple *authoring* report still wants it even when the source has no tag; `mediastreamvalidator` did not. |
| RTMP AMF `onCaption` / `onTextData` | **done** | Drain-before-freeze. HTTP e2e WebVTT rendition. |
| H.264 SEI CEA-608/708 declaration | **done** | Scanner kept; proven on AVCC from native RTMP. |
| HEVC/AV1 in-band captions | **gap** | Detector is H.264-only (same as avformat). |
| Container WebVTT / SubRip / MovText | **gap** | Muxer still converts SubRip; MPEG-TS subtitle PIDs are skipped; nothing ingest feeds WebVTT/SubRip. |
| WebVTT cue id/settings, SubRip position | **gap** | Lived on FFmpeg packet side data. |

## Timing (two different elst cases)

Delayed start and encoder priming are not the same edit list.

| Feature | Status | Notes |
|---|---|---|
| Different first PTS per track (RTMP and MPEG-TS) | **done** | Each track records `first_pts` from its first sample. Mux rebases by a shared `presentation_origin_pts`. A later-starting track gets an empty edit (`media_time = -1`) and a positive chunk `media_start`. Covered by `audio_and_video_keep_their_relative_offset_through_packaging` and `a_genuine_later_video_start_is_exposed_as_positive_media_start`. RTMP discovery asserts video `first_pts = 0`, audio `21`. |
| AAC skip-samples / `initial_padding` | **mapped** (mux) / **gap** (ingest) | Mux writes priming into `elst` when `AudioTiming.initial_padding_samples` or packet `AudioTrim` is set. Adapters leave both default. Relevant for **file-originated AAC** (MP4/MKV encoder delay, often 1024 or 2112 for HE). **Live RTMP/TS AAC-LC almost never carries skip-samples**; a later first audio PTS is delayed start, not priming. |
| Opus `pre_skip` | **gap** (ingest) | `OpusHead` bytes 2–3. Always relevant for Opus (typical 312 samples @ 48 kHz). Parser reads channels/rate only. Mux `dOps` can take `initial_padding` once ingest fills it. |
| Declared `video_delay` (B-frame reorder depth) | **gap** | Always 0. B-frames still work from PTS/DTS (TS) or composition offset (RTMP). Normalizer hold-back from container `video_delay` is unused. |

## CMAF writer extras avformat got for free

| Feature | Status | Notes |
|---|---|---|
| Init + fragments, no `sidx`, delayed `moov` | **done** | |
| Edit lists: priming, delayed start, composition offset | **done** | |
| `colr` / `pasp` / HDR (`mdcv`/`clli`) | **untested** / likely **gap** | Native box builders do not write these. Validator did not flag the H.264 SDR fixture. |

## Live proof (not unit mapping)

| Publish | Result |
|---|---|
| `ffmpeg -re -c copy -f flv` H.264 High@L4 1080p24 GOP 2s, AAC-LC 48 kHz stereo → RTMP | Playable LL-HLS. `mediastreamvalidator -t 30`: HTTP/2 only once `--http-public-url` is set. |
| HEVC / AV1 / Opus live | Not run. |
| Enhanced RTMP second audio/video | HTTP e2e: two OneTrack AAC-LC variants. Apple HLS HTTPS: `rtmp_multitrack_h264_two_aac`. |
| SRT MPEG-TS live | Apple HLS HTTPS: `srt_h264_aac` (one H.264 + one AAC). Dual-audio TS is the in-process MPEG-TS case, not a second SRT matrix row. |
| MPEG-TS extra audio PIDs | Apple HLS HTTPS: `mpegts_h264_two_aac`. |
| File dump without `-re` | Out of scope here; 16 MiB queues were the old metronome. |
