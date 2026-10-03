# Native ingest vs avformat — feature parity

Status of what the FFmpeg/avformat path used to do, after the native RTMP +
MPEG-TS adapters and transmux CMAF writer. Delivery, HLS projection, pacing,
and store are unchanged and are not listed here.

Legend: **done** (wired and covered), **extractable** (bytes already in hand,
not copied onto `DiscoveredTrack`), **mapped** (codec/box path exists, no live
or HTTP proof), **gap** (dropped or refused), **untested**.

Items marked **gap** below are not leftover FFmpeg replacement work. They are
new codecs, containers, or caption walks that avformat happened to probe. Do
not build them until a publisher needs them.

## Ingest containers

| Feature | Status | Notes |
|---|---|---|
| RTMP / Enhanced RTMP, one video + one audio | **done** | Native `rtmpx` → `Packet`. Live `mediastreamvalidator` on H.264 + AAC-LC. |
| SRT MPEG-TS | **done** | `rsrt` (pure Rust, IPv4) into `StreamingTsDemux`. Unit fixture is H.264 + ADTS AAC. |
| SRT Matroska / WebM | **gap** | EBML magic refused on purpose. MPEG-TS only. |
| SRT FLV, MP4/fMP4, anything else avformat probed | **gap** | MPEG-TS only. Not an incomplete demuxer; other containers were never the live ingest contract. |
| Enhanced RTMP extra A/V tracks | **done** | OneTrack and packed `ManyTracks` become distinct catalog entries keyed by `audio/{id}` / `video/{id}`. Legacy default track stays `audio` / `video`. Identical sequence headers are ignored. Changed codec configurations fail the publish; a new id after freeze is `TrackSetChanged`. HTTP e2e: two OneTrack AAC → two `STREAM-INF` audio variants. |
| MPEG-TS extra audio/video PIDs | **done** | Mapper keeps every mapped PID. Admission default is `tracks = Any`. Apple HLS: H.264 + two AAC-LC PIDs, and two H.264 PIDs + AAC. HTTP e2e covers the two-video ladder through pre-roll cadence. |
| Media over QUIC | **done** (native client tested) | WebTransport and raw QUIC, moq-lite-06 and moq-lite-05. LOC and legacy Hang. H.264 (including Annex-B avc3), HEVC/AV1 with catalog configuration, AAC/Opus. Catalog changes and media-group gaps fail ingestion. moq-cli 0.10 (moq-lite-05) and 0.13 (moq-lite-06) H.264/AAC → HLS decoded with FFmpeg. Embedded CEA-608 in H.264 is declared (moq-cli `avc3` import over QUIC, and an in-process test). Browser publishing has a manual Chrome procedure (`tools/browser-moq-publish.html`, `tools/browser-ladder-bench.html`) but no CI job. See [MOQ ingestion](../../moq.md). |

## Codecs

| Feature | Status | Notes |
|---|---|---|
| H.264 + AAC-LC, RTMP | **done** | HTTP e2e + Apple validator (cleartext HTTP/1.1 → HTTP/2 warning only). |
| H.264 + AAC, MPEG-TS | **done** | Discovery + CMAF round-trip in-process. The MPEG-TS Apple matrix passes. |
| HEVC | **done** (Apple matrix) | Apple matrix passes for RTMP HEVC + AAC and MPEG-TS HEVC + AAC. |
| AV1 | **done** (decode + live start-of-stream) | RTMP mapping and CMAF writer exist. TS accepts GStreamer AV1G private PES with its av1C descriptor and an in-band sequence header. Native round-trip and independent FFmpeg decode pass. A live AV1+Opus publish plays in Chrome and hls.js. Other AV1 TS mappings remain unsupported. |
| FLAC over Enhanced RTMP | **done** (mono/stereo, decoder tested) | Exact frame durations, packed-message splitting, strict/GAP policy, and fMP4 output. FFmpeg PCM comparisons pass. Browser playback remains unverified. See [FLAC](../gap-validation/flac.md). |
| Opus over RTMP | **done** (in-process) | A stereo silence packet passes RTMP parsing, normalization, CMAF writing, and demuxing. The test checks 312-sample pre-skip, 48 kHz playback, and separate 44.1 kHz input metadata. Live RTMP Opus is untested; live MPEG-TS Opus is proven in Chrome/hls.js with AV1. |
| Opus over MPEG-TS | **done** (mono/stereo, decode + live start-of-stream) | Resolves the Opus PMT descriptors and parses PES control headers before shared normalization. Preserves startup and final trim at 48 kHz. Extended channel mappings remain unsupported. |
| AAC-HE / HEv2 frame size | **done** (signaled SBR/PS) | Shared ASC parser handles hierarchical and sync-extension signaling, core/output sample rates, and 960/1024 core frames. Real HE and HEv2 FLV fixtures yield 2048 output samples at 48 kHz and decode through FFmpeg. Implicit SBR without ASC signaling still needs bitstream detection. |
| MPEG-TS AAC-LATM | **gap** | Avformat refused the publisher. Native skips the PID (video-only publish possible). Needs an LATM→raw AAC unframer before the shared ASC path. Skip until a publisher sends LATM. |
| AC-3 / E-AC-3 / MP3 / VVC / VP9 | **gap** | Unmapped; admission would refuse them anyway. New codec work, not unfinished native ingest. |

## Language, title, captions

| Feature | Status | Notes |
|---|---|---|
| MPEG-TS ISO 639 (PMT `ISO_639_language_descriptor`, tag `0x0A`) | **done** | First three-letter code on `TrackSpec.es_info_descriptors` becomes `DiscoveredTrack.language`. Catalog canonicalizes (`eng` → `en`, `spa` → `es`). Covered by `discovers_iso_639_language_on_each_audio_pid`. |
| RTMP `onMetaData` language | **done** | Catalog freeze applies per-track language, then audio/video-specific and generic metadata. Empty, oversized, or control-containing text is ignored. |
| Track `title` | **done** (RTMP) / **gap** (TS) | Uses the same bounded metadata precedence as language. MPEG-TS service names and `stream_identifier_descriptor` titles are not copied onto `DiscoveredTrack`. Skip until a TS publisher needs `NAME` on `EXT-X-MEDIA`. |
| HLS `LANGUAGE` on `EXT-X-MEDIA` | **done** | Both native adapters supply source language when available. Catalog canonicalization and HLS projection remain shared. |
| RTMP AMF `onCaption` / `onTextData` | **done** | Drain-before-freeze. HTTP e2e WebVTT rendition. |
| H.264 SEI CEA-608/708 declaration | **done** | Scanner kept; proven on AVCC from native RTMP. |
| HEVC in-band captions | **done** (unit tests) | Same A53 CEA-608/708 payload as H.264, walked in HEVC prefix SEI (type 39) and suffix SEI (type 40). `hvcC` length-prefix and Annex B are both covered. `CaptionVerifier` treats HEVC like H.264, including mixed H.264+HEVC ladders. |
| AV1 in-band captions | **gap** | Different walk than HEVC SEI. See [In-band captions beyond H.264](#in-band-captions-beyond-h264). |
| Container WebVTT / SubRip / MovText | **gap** | Muxer still converts SubRip; MPEG-TS subtitle PIDs are skipped; nothing ingest feeds WebVTT/SubRip. |
| WebVTT cue id/settings, SubRip position | **gap** | Lived on FFmpeg packet side data. |

## Timing (two different elst cases)

Delayed start and encoder priming are handled differently. A delayed start is a later `tfdt` on the shared clock. Only media that decodes before the shared origin (priming, or a reordered picture's earlier DTS) shifts the track's media clock, through one non-empty edit. The muxer never writes an empty edit, because hls.js and Shaka read sample times from `tfdt` and Chrome's MSE ignores empty edits.

| Feature | Status | Notes |
|---|---|---|
| Different first PTS per track (RTMP and MPEG-TS) | **done** | Each track records its first presentation timestamp. Audio adds declared priming to the encoded timestamp. RTMP video includes its composition offset. Mux rebases by a shared `presentation_origin_pts`. A later-starting track keeps a later `tfdt` and a positive chunk `media_start`, with no empty edit. Covered by `audio_and_video_keep_their_relative_offset_through_packaging`, `a_later_video_start_is_carried_by_tfdt_not_an_empty_edit`, `a_later_audio_start_needs_no_edit_even_with_priming`, and `http_cmaf_timestamps_and_pdt_share_one_clock_without_empty_edits`. RTMP discovery asserts video `first_pts = 0`, audio `21`. |
| AAC skip-samples / `initial_padding` | **mapped** (mux) / **gap** (ingest) | Mux writes priming into `elst` when `AudioTiming.initial_padding_samples` or packet `AudioTrim` is set. AAC adapters leave both default. Relevant for **file-originated AAC** (MP4/MKV encoder delay, often 1024 or 2112 for HE). **Live RTMP/TS AAC-LC almost never carries skip-samples**; a later first audio PTS is delayed start, not priming. Skip until file-origin AAC is an ingest source. |
| Opus `pre_skip` | **done** (RTMP and mono/stereo TS) | `OpusHead` bytes 10–11 contain little-endian pre-skip, after the eight-byte signature. The shared parser preserves gain and channel mapping. Normalization emits leading trim once. CMAF writes `dOps` version 0 and an edit that selects past priming. On a delayed track, priming presents just before the audible start, as in Apple's segmenters: an edit can only hide media before presentation zero. TS takes priming from its first PES control header. |
| Declared `video_delay` (B-frame reorder depth) | **done** (H.264/HEVC) | Shared SPS parsers read H.264 VUI reorder limits and HEVC sub-layer ordering. Real B-frame fixtures cover both. Missing declarations retain zero; AV1 delay inference remains a gap. |

## CMAF writer extras avformat got for free

| Feature | Status | Notes |
|---|---|---|
| Init + fragments, no `sidx`, delayed `moov` | **done** | |
| Edit lists: priming, delayed start, composition offset | **done** | At most one non-empty edit per track, for priming or a composition offset before the origin. A delayed start is a later `tfdt`. Regression tests cover delayed audio with priming, delayed video, and the PDT-to-`tfdt` mapping over HTTP. Packet-only leading trim also reaches the edit list. Opus final trim shortens the last fragment sample duration. |
| Opus random-access pre-roll | **done** (boxes) | Init roll descriptions and per-fragment sample groups count enough preceding packets for 80 ms, including variable durations across fragment boundaries. Startup uses a conservative description when history is unavailable. Unit tests check roll distances. Live start-of-stream playback is proven; mid-window join and DVR seek against those roll groups are not separately proven. |
| `colr` / `pasp` / HDR (`mdcv`/`clli`) | **done** (H.264/HEVC startup) | SPS/VUI supplies color and aspect. Static HDR SEI seen before init emission supplies mastering display and content light boxes. Real fixtures check serialized values. AV1 visual metadata and changes after init remain gaps. |

## Publisher disconnect (SRT)

libSRT never told the application whether the peer sent `SHUTDOWN` or the socket
just went quiet. Rushls therefore omitted `#EXT-X-ENDLIST` on every SRT end, so
a crash could not tear down players that the reconnect budget still covers.

`rsrt` does distinguish those cases:

| `rsrt::CloseReason` | Mapped `InputState` | Playlist |
|---|---|---|
| `Shutdown` (peer sent SHUTDOWN) or `Local` | `Closed` | `#EXT-X-ENDLIST` |
| `PeerIdle`, `DataIdle`, `SequenceDiscrepancy` | `Interrupted` | live playlist kept open |

`SrtSocket::recv` returns `Ok(None)` for SHUTDOWN/Local and `Err(Closed(reason))`
for a break. Dropping an `rsrt` handle sends SHUTDOWN, which is the orderly
encoder stop (FFmpeg/`srt-live-transmit` `srt_close`, Ctrl-C). `kill -9` or a
network cut produces `PeerIdle` after `ingest.idle_timeout` (default 10 s). An encoder
that stays connected and only stops sending media will keep the socket alive
with keepalives; that is transport silence vs media silence, and the live stall
policy already covers the latter.

Covered by `peer_shutdown_closes_the_source_and_idle_breaks_interrupt_it`, by
the MPEG-TS-over-SRT fixture ending `InputState::Closed` after the caller is
dropped, and by HTTP e2e `a_graceful_srt_disconnect_appends_endlist`, which
fetches a media playlist after `SessionOutcome::Ended` and asserts
`#EXT-X-ENDLIST`.

## Live proof (not unit mapping)

| Publish | Result |
|---|---|
| `ffmpeg -re -c copy -f flv` H.264 High@L4 1080p24 GOP 2s, AAC-LC 48 kHz stereo → RTMP | Playable LL-HLS. `mediastreamvalidator -t 30`: HTTP/2 only once `--http-public-url` is set. |
| HEVC live | Apple matrix passes for HEVC RTMP and MPEG-TS. |
| AV1 + Opus live MPEG-TS | ~30 s start-of-stream plays in Chrome and hls.js. `mediastreamvalidator` reports an unrecognized codec: Apple's validator does not certify AV1 or Opus in HLS, so it cannot replace that player proof. Mid-window join and DVR seek are not separately recorded. |
| Enhanced RTMP second audio/video | HTTP e2e: two OneTrack AAC-LC variants. Apple HLS HTTPS: `rtmp_multitrack_h264_two_aac`. |
| SRT MPEG-TS live | Apple HLS HTTPS: `srt_h264_aac` (one H.264 + one AAC). Dual-audio TS is the in-process MPEG-TS case, not a second SRT matrix row. |
| MPEG-TS extra audio PIDs | Apple HLS HTTPS: `mpegts_h264_two_aac`. |
| MPEG-TS extra video PIDs | Apple HLS HTTPS: `mpegts_h264_two_video`. HTTP e2e: two H.264 variants plus shared AAC. |
| File dump without `-re` | Bounded queues apply backpressure after pre-roll. A closed RTMP input drains all discovery packets across batch boundaries. |

## In-band captions beyond H.264

`H264CaptionDetector` answers a playlist question: does this track carry ATSC
A53 CEA-608/708, and on which channels? It does not rewrite the bitstream. The
type name is historical: H.264 and HEVC both construct a detector. Any other
video codec counts as unverifiable, so a mixed H.264+AV1 ladder never declares
captions even if the H.264 rendition carries them.

**HEVC** is implemented. CEA-608/708 still travel as ITU-T T.35 inside SEI;
`scan_sei` / `DtvccAssembler` are shared. The HEVC walk is:

- An `hvcC` / Annex-B splitter. HEVC NAL headers are two bytes; prefix SEI is
  type 39 and suffix SEI is type 40, not H.264 type 6. Only the base layer
  (`nuh_layer_id == 0`) is inspected.
- `CaptionVerifier` constructs a detector for HEVC, so an HEVC-only publish can
  declare and a mixed H.264+HEVC ladder can reconcile observations.

**AV1** is not SEI. ATSC A/343 puts captions in Metadata OBUs (`obu_type` 5)
carrying ITU-T T.35. The OBU walker already used for sequence headers and
keyframes can locate those OBUs; the T.35 body then joins the same A53 parser.
A reduced-header AV1G stream still has to expose metadata OBUs in-band.

AV1 captions are not required for native ingest parity with the H.264-only
avformat path we replaced.

## Adapter boundaries and backpressure

`rtmpx` exposes `ElementaryUnit` and `ElementaryCodec` through `visit_elementary_units()`.
This API removes FLV framing and preserves decoder configuration bytes.
It does not choose a media container or write CMAF boxes.
Rushls owns codec interpretation, timestamp normalization, and packaging.
Rushls uses RTMPX 3 from crates.io through the workspace dependency.
The transport pulls one session output at a time and writes control packets with a resumable cursor.
Socket reads become owned buffers; the decoder retains their slices while it assembles messages.
Rushls coalesces fragmented media once at its contiguous codec-input boundary.
A message that already occupies one contiguous slice keeps that allocation.
Elementary-unit visitors avoid a temporary result vector for each media message.
Socket deadlines belong to Rushls's `RtmpTimeouts`; RTMPX remains sans-I/O.

RTMP and MPEG-TS share input-limit validation and first-presentation timestamp accounting.
They retain separate discovery state machines because RTMP sequence headers and TS PMT events have different lifecycles.
Both adapters are async on Tokio. RTMP uses a byte-budgeted ingress queue because the
RTMP session pushes tags. MPEG-TS demuxes on a Tokio task that pulls from SRT, so the
rsrt receive queue is drained even while the live pacer is not reading packets.
The demux task is not a blocking thread.

RTMP limits queued bytes and caps the queue at 4096 events.
Terminal state remains observable after the queue drains.
Shutdown wakes waiting readers and writers. The last writer drop interrupts the reader.
The source also limits non-media events per fill, so continuous metadata cannot monopolize a worker.

Discovery has byte and wall-time limits, including the RTMP drain before catalog freeze.
An RTMP probe-byte overflow fails discovery instead of consuming and silently dropping the packet that crosses the limit.
A deadline cannot freeze a catalog without timestamps on all discovered A/V tracks.
Pre-roll then consumes the same source under its own limits to establish segment cadence.
The live pacer starts after pre-roll. Queue backpressure does not require a larger pre-roll queue.

The configurable shared publisher budget describes accounted application buffers, not maximum process memory.
The native TS demuxer has additional per-PID PES and probe buffers.
Parsed RTMP objects, queue entries, and allocator overhead also consume memory.

The Opus rules come from [RFC 7845](https://www.rfc-editor.org/rfc/rfc7845.html)
and [Opus in ISOBMFF](https://www.opus-codec.org/docs/opus_in_isobmff.html).
The latter distinguishes startup trimming from random-access pre-roll.

## Regression fixtures and validation

The new synthetic fixtures under `tests/apple_hls/fixtures` cover HE/HEv2,
Opus TS, AV1G TS, H.264 aspect/color, and HEVC static HDR.
See [fixture generation notes](../../../tests/apple_hls/fixtures/native-codecs.md).
The ordinary library suite requires no external decoder for these fixtures.
`independent_decoder_accepts_native_ts_opus_and_av1g` is an ignored test that requires FFmpeg.
Run it explicitly to check decoding and Opus startup alignment against the TS decoder.
FFmpeg currently emits the 648 padded final samples from the fragmented Opus fixture.
The serialized final duration is correct, but this decoder does not apply that end trim.
The regression checks the full audible prefix against the independently decoded source.
The existing Apple HLS matrix also passes after these changes.
Apple `mediastreamvalidator` cannot certify AV1 or Opus HLS; Chrome and hls.js are the live proof for those codecs.
