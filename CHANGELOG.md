# Changelog

All notable changes to Rushls are listed here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and versions follow
[Semantic Versioning](https://semver.org/). Before 1.0, a minor version can
contain breaking changes; they are listed first.

## [Unreleased]

### Added

- The container image is published for Linux ARM64 as well as AMD64, under
  the same tags.

### Fixed

- The release workflow publishes only the workspace crates whose version is
  not yet on crates.io. In 0.2.0, the unchanged `rushls-common` stopped the
  automatic upload, and `rushls` 0.2.0 was published by hand.

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

[Unreleased]: https://github.com/darfink/rushls/compare/v0.2.0...HEAD
[0.2.0]: https://github.com/darfink/rushls/compare/v0.1.0...v0.2.0
[0.1.0]: https://github.com/darfink/rushls/releases/tag/v0.1.0
