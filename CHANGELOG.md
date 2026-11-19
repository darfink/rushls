# Changelog

All notable changes to Rushls are listed here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and versions follow
[Semantic Versioning](https://semver.org/). Before 1.0, a minor version can
contain breaking changes; they are listed first.

## [Unreleased]

## [0.3.1] - 2026-11-19

### Changed

- Updated documentation, configuration reference, and `rushls-common` README to fix stale CLI/TLS/interpolation references and unify copywriting across user guides.
- Aligned `examples/player/index.html` with the quick-start stream path (`live/event`).

## [0.3.0] - 2026-11-18

### Breaking changes

- To receive publisher hooks, return a public `stream_id` and `principal`
  from admission. If either contains the publish credential, Rushls withholds
  the publisher hooks. This includes open RTMP admission. Playback URLs still
  work, and reconnects inherit the restriction until the stream expires.
- For Rust integrations, use `media::start_normalizer` and `mux::start_muxer`.
  These functions replace the factory traits and their fields on `Services`.
  Custom publisher factories now implement `store` and `start_if_current`.

### Added

- The container image is published for Linux ARM64 as well as AMD64, under
  the same tags.

### Fixed

- Publisher hooks redact credential-bearing resource names and omit
  credential-bearing namespaces. Admission still receives the original values.
- A publisher that finishes startup after a takeover can no longer revoke
  the new publisher's write lease.
- After an RTMP connection drops, a publisher can reconnect to the same HLS
  playlist. An explicit unpublish still ends the playlist with `EXT-X-ENDLIST`.
- Retiring a rendition deletes its spilled segment files, including files from
  writes still in progress at retirement. Retained byte counts reflect the removal.
  Each stream has its own spill directory, so retiring one stream preserves
  media for other streams with nested names.
- Live playlist eviction preserves at least three advertised target durations
  of media. A request for a GAP part returns HTTP 404 even if the client
  requested it early through a preload hint.
- Segment boundaries preserve fractional frame rates declared by the codec,
  such as 24000/1001 frames per second.
- MoQ discovery continues when an optional caption track closes. The
  certificate fingerprint now follows the active QUIC certificate, including
  when the listener rejects a replacement.
- The browser A/V sync check now detects whole-second offsets between flashes
  and beeps.
- The release workflow publishes only the workspace crates whose version is
  not yet on crates.io. If a version lookup fails, the workflow stops before
  uploading any crate. In 0.2.0, the unchanged `rushls-common` stopped the
  automatic upload, and `rushls` 0.2.0 was published by hand.

### Known limitation

- Stock hls.js 1.7.3 can skip a playable part and stall before the end of a
  live stream. The required Chrome playback check uses the
  [locally patched player](https://github.com/darfink/rushls/blob/v0.3.0/tools/patches/hls.js/README.md).

## [0.2.0] - 2026-10-04

### Breaking changes

- Lifecycle hook events are renamed from `session.*` to `publisher.*`.
  Update hook receivers that match on the old names.
  See [admission and hooks](https://github.com/darfink/rushls/blob/main/docs/admission-and-hooks.md) for when each event is sent.
- MoQ ingest uses moq-net 0.3 and accepts moq-lite-05 and moq-lite-06 only.
  Publishers on older moq-lite drafts are refused.

### Fixed

- Audio and video now play in sync when the tracks start at different times
  or the video has B-frames. Before, players read a later audio start or a
  composition offset incorrectly: audio that started late played 113–180 ms
  early, and B-frame video was 58–67 ms off, in hls.js, Shaka and Safari.
  Each track's first `tfdt` now sits where the track starts on the shared
  timeline, with no empty edit lists. Measured drift is now within 10 ms.
- A stream whose audio starts after its first video keyframe no longer fails
  after its second segment. Later audio segments follow the video cadence.

### Added

- MoQ ingest accepts moq-lite-06.
- The hooks documentation describes when each event is sent.
- `tools/av-sync/check-av-sync.py` measures A/V sync in real browsers.
  CI runs it in Chrome with hls.js and Shaka; Safari runs locally.

### Changed

- Dependencies are updated, including tower-http 0.7.

## [0.1.0] - 2026-10-02

First release.

[Unreleased]: https://github.com/darfink/rushls/compare/v0.3.1...HEAD
[0.3.1]: https://github.com/darfink/rushls/compare/v0.3.0...v0.3.1
[0.3.0]: https://github.com/darfink/rushls/compare/v0.2.0...v0.3.0
[0.2.0]: https://github.com/darfink/rushls/compare/v0.1.0...v0.2.0
[0.1.0]: https://github.com/darfink/rushls/releases/tag/v0.1.0
