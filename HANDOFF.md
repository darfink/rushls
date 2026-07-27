# RushLS handoff

## Built

- FFmpeg pass-through CMAF muxing with one MP4 context per audio/video track.
  It uses `cmaf+dash+skip_sidx+frag_custom+delay_moov`, enforces exact negotiated
  timebases, emits delayed initialization before the first chunk, supports
  strict or random-access-extended segment boundaries, and keeps muxing
  track-local.
- Pure-Rust, track-local WebVTT packaging alongside CMAF through
  `PassThroughMuxerFactory`. Native WebVTT cue metadata survives ingest and
  normalization; SubRip is converted with safe common styling, entity
  decoding, fixed-window duplication, empty gap segments, and bounded work.
- Shared crate-private FFmpeg helpers for errors, dictionaries, rationals,
  packets, audio skip metadata, AVIO ownership, and zero-copy source packet
  payload ownership where FFmpeg provides a real `AVBufferRef`.
- Audio priming and genuine track start offsets preserved through AVFormat,
  normalization, segmentation, CMAF edit lists, chunk timing, and the HLS
  store. Presentation origin and segmentation origin remain separate.
- Production pass-through media normalization:
  - video duration derivation, bounded DTS synthesis, 90 kHz projection, and
    drift-free fractional cadence;
  - sample-clock audio timing with exact priming, cached timestamp tolerance,
    and discontinuity/padding consistency validation;
  - subtitle timing projected to 90 kHz for future `X-TIMESTAMP-MAP` rendering.
- Normalization was split by responsibility: pipeline samples, presented
  timing, contracts, and per-kind pass-through implementations are separate.
- Enhanced RTMP ingest through `scuffle-rtmp`. Admission pauses the publish
  command, accepted audio/video/AMF0 messages are framed as a byte-bounded FLV
  stream, and the existing AVFormat source performs discovery and demuxing.

## Primary files

- FFmpeg/CMAF:
  - `src/ffmpeg/{audio,dictionary,error,packet,rational,subtitle}.rs`
  - `src/mux/cmaf/{mod,ffi}.rs`
  - `src/mux/{mod,passthrough,track,webvtt,fixtures}.rs`
  - `src/mux/webvtt/subrip.rs`
- Priming and source metadata:
  - `src/domain/{track,time,mod}.rs`
  - `src/source/{packet,limits}.rs`
  - `src/source/avformat/{ffi,metadata,source,fixtures}.rs`
- RTMP transport:
  - `src/source/transport/rtmp.rs`
  - `src/source/avformat/channel.rs`
- Timeline and segmentation:
  - `src/media/{timeline,validate,fixtures}.rs`
  - `src/segment/{mod,preroll,boundary}.rs`
  - `src/segment/boundary/part.rs`
- Normalization organization:
  - `src/media/{mod,sample,timing}.rs`
  - `src/media/normalize/mod.rs`
  - `src/media/normalize/passthrough/{mod,audio,video,subtitle,tests}.rs`
- Integration and regression coverage:
  - `src/session/{mod,tests}.rs`
  - `src/delivery/hls/store/tests.rs`
  - `Cargo.toml` and `Cargo.lock`

The CMAF and priming work is recorded in commits `0835c27` and `1178d41`.
The normalization module split and WebVTT/SubRip packaging are currently
working-tree changes. Preserve unrelated existing edits in `TODO.md`.

## Validation

- `cargo test`: 316 passed.
- `cargo clippy --all-targets -- -D warnings`: passed.
- `cargo fmt --all -- --check`: passed.
- `git diff --check`: passed.

## Important contracts

- Incoming and mux output timebases must match exactly after normalization;
  FFmpeg timebase changes are rejected rather than silently rescaled.
- Video and subtitles normalize to 90 kHz. Audio normalizes to its decoded
  sample-rate clock; Opus must use 48 kHz.
- WebVTT and SubRip both publish as `wvtt` direct media segments. Cue timestamps
  stay publication-relative, gaps become empty segments, and coordinate-based
  SubRip cues are rejected until a rendering canvas can be declared.
- Absolute timestamps and observed intervals use checked endpoint projection.
  Only synthesized cadence uses the stateful rational accumulator.
- Matching nonzero first-packet leading trim and track initial padding is
  required while publications always begin at stream start. Seeking or
  mid-stream starts will require explicit trim provenance.
- Subtitle initialization is a WebVTT header with `X-TIMESTAMP-MAP`; future HLS
  rendering must expose it with `EXT-X-MAP`.

## HLS projection and HTTP serving

Built on top of the above. See `TODO.md` for what remains open.

- An immutable `PlaylistContract` per rendition (target duration, part target,
  format), frozen at creation. A reconnect changing cadence fails both
  `PackagedRendition::compatible_with` and contract equality, so it gets a new
  durable rendition while the old playlist retires with an `ENDLIST` rather than
  changing terms under viewers already reading it.
- `SegmentBoundaryPolicy::ExtendToRandomAccess` carries a mandatory
  `maximum_extension`, and `RenditionConfig::maximum_segment_duration`
  advertises planned + budget. Exceeding it fails the publication in the muxer:
  an over-long segment's parts are fetchable before its completion is known, so
  no later rejection can contain the overrun.
- Durable discontinuity state — `last_parent_publication`,
  `discontinuity_before` on `StoredSegment` *and* `OpenSegment`, an evicted-tag
  `discontinuity_sequence`, and an explicit `media_sequence`.
- Transactional duration validation: segment against the fixed target, part
  against `PART-TARGET`, and the 85% floor checked on a part's predecessor when
  a successor makes it non-final.
- `delivery::hls::project` — pure snapshot-to-playlist projection.
  Presentation-wide `EXT-X-SERVER-CONTROL`, open-segment tags before the first
  `EXT-X-PART`, format-driven resource naming (`.m4s`/`.vtt`), and
  largest-playable-sum `BANDWIDTH` with a codec union.
- `delivery::hls::serve` — resolution, blocking reload, deadlines, multi-frame
  media bodies, and a render cache keyed on the catalog revision *and every
  sibling's* edge revision, since rendition reports cross renditions.
- `server::http` — axum over HTTP/1.1 and h2c, byte ranges spanning stored part
  buffers, HEAD, CORS, graceful shutdown. TLS is left to a terminating proxy.

## Suggested next steps

1. Implement and validate the SRT publishing adapter, reusing the byte-bounded
   AVFormat bridge introduced for RTMP.
2. Wire production services in `main.rs`: authenticator, normalizer,
   pass-through muxer, HLS publisher/store, HTTP server, session registry,
   metrics, and shutdown/maintenance tasks — including `StreamStore::maintain`
   and `Origin::prune` on one timer.
3. Add end-to-end tests covering AVFormat → normalization → CMAF/HLS → HTTP with
   `ffprobe` and, where available, Apple media validation tooling.
