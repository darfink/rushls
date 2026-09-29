# Rushls TODO

The core media pipeline is implemented within the limits documented here.
Production readiness still requires operational validation.
Future features are not release requirements unless a deployment needs them.

## Release validation and hardening

Complete these checks before a broad production rollout:

- [x] Extend the soak harness with real media downloads, slow consumers, CPU/RSS,
  process allocation counters, latency limits, resource bounds, and cleanup assertions.
- [x] Exercise concurrent reconnects, malformed RTMP connections, abrupt publisher
  loss, recorder write failure/recovery, and DVR pressure alongside healthy streams.
  Record the executed workloads and results in [load validation](docs/development/load-testing.md).
- [ ] Establish deployment-specific capacity through longer runs on representative
  hardware and media. Include real disk-full/stalled-device and packet-loss scenarios
  where the deployment requires them. Local validation is not a production capacity claim.
- [x] Review ELST/PDT offsets and reconnect fetch grace. Add HTTP regressions for
  real CMAF A/V offsets and priming, part/segment/init fetch deadlines after reconnect,
  and blocking reloads across successor publication and rendition retirement.
- [ ] Complete the supported codec/player matrix for startup, seeking, live playback,
  and rendition switching around GAPs. Record transient freezes and skips separately
  from persistent stalls. Track results in the [player matrix](docs/development/gap-validation/README.md)
  and use the [GAP regression checks](docs/development/gap-validation/regressions.md).
- [ ] Aggregate hook failure logs into unhealthy/recovered transitions and periodic
  totals. Preserve per-event counters. Include a rejecting endpoint under
  `segment.ready` traffic in the load tests.
- [ ] Decide where publisher fairness is enforced: application admission or deployment
  infrastructure. Document and test the chosen protection against one publisher
  exhausting global capacity, including input without pacing.
- [x] Automate formatting, lints, tests, native binary packaging, and tagged release delivery.
  Apple validation and all native archive smoke tests block publication.
  See [release procedures](docs/development/releasing.md).
- [x] Add MIT licensing and scan tracked files and reachable Git history for credentials.
  See the [review scope and findings](docs/development/reviews/credentials-2026-09-23.md).
- [ ] Verify the final Windows storage cleanup fix and extracted ZIP smoke test on CI.
  The previous native run passed 1,009 Rushls tests; one cleanup race required a fix.
  GitHub blocked the verification run because of account billing or spending limits.
- [ ] Publish the first version tag after the five-target archive matrix passes.
- [ ] Submit the hls.js pending-part correction upstream and track its release.
  The official player's clean control also fails; this is not limited to faulty publishers.


## Known limitations and unresolved reports

These limits remain explicit in the supported scope:

- Release binaries cover Linux and macOS on x86-64 and ARM64, plus Windows x86-64.
  Windows recording requires hard-link support and has the documented directory-metadata durability limitation.
- Official hls.js 1.7.3 can stall on valid live streams around rendition switches and ENDLIST.
  Required Chrome validation uses the documented local player patch pending upstream adoption.
- GStreamer 1.28 passes clean live playback. GAP recovery remains a failing diagnostic;
  its parser expects a colon after EXT-X-GAP, which is a likely cause.

- Safari video-only playback can end prematurely at a video GAP. Tested A/V cases
  continued, sometimes after a freeze until the next IDR. This does not establish
  support for every codec, player, or seek position.
  See [video GAP validation](docs/development/gap-validation/video.md).
- Permissive video GAP recovery supports declared fixed-cadence H.264, single-layer
  HEVC, and single-layer AV1 without presentation reordering. Unsupported video
  recovery mappings remain fatal. HEVC and AV1 GAPs are validated by FFmpeg decoding,
  not yet in the browser/Safari player matrix.
- Midstream codec/configuration changes and unexplained clock resets remain fatal.
  Reconnect discontinuities do not provide recovery within an active publication.
- Different initial track epochs remain an unresolved input case. Independent
  timestamp rebasing can erase legitimate A/V offsets. There is no automatic
  inference that distinguishes those cases.
- MoQ records the direct QUIC peer address. A relayed publisher's original address
  requires a separate trusted attribution mechanism.
- The reported subtitle collision needs a reproducible fixture. Replacement and
  coalescing tests already cover some overlapping-cue cases. The report is not
  yet a confirmed remaining defect.

## Future features and design decisions

Evaluate these separately from release validation:

- [x] Port recording and store ownership to Windows, with native tests and ZIP packaging.
- [ ] Add signed/notarized release binaries and evaluate static Linux builds.
- [ ] Add multi-architecture container publication.

- [ ] Support coordinated midstream discontinuities and a defined subset of compatible
  codec/configuration changes. Define timestamp-reset semantics before adding recovery.
- [ ] Extend video GAP recovery to presentation reordering and other mappings with
  decoder and player validation.
- [ ] Support AV1 SVC.
- [ ] Evaluate trusted original-publisher attribution for relayed MoQ sources.
- [ ] Add H.264 profile/level constraints to codec admission if deployments need them.
- [ ] Decide whether policy or connection parameters can declare closed-caption presence.
- [ ] Decide whether to expose a mode that disables LL-HLS parts. Full-segment playback
  already works with the current output.
- [ ] Evaluate additional content-type parameters, such as codecs or charset.
- [ ] Review which authoring recommendations can produce warnings instead of admission
  failures. Preserve mandatory format constraints and truthful output declarations.
- [ ] Audit specification-dependent behavior and add precise references beside non-obvious
  decisions. Separate HLS requirements, Apple authoring requirements, and CMAF constraints.
- [ ] Profile performance and allocations before selecting optimizations.
- [ ] Review error types for consistent classification and actionable diagnostics.
- [ ] Support HLS interstitials: `EXT-X-DATERANGE` with `CLASS="com.apple.hls.interstitials"`
  and `X-ASSET-URI` / `X-ASSET-LIST`. The player fetches each asset as its own HLS
  presentation, so the main stream's codecs are not a hard requirement; matching encodes
  only make transitions smoother. Needs a cue source (an HTTP "insert now / at time" API
  first; SCTE-35 over MPEG-TS for broadcast sources; no mainstream encoder is confirmed to
  send RTMP `onCuePoint`), DATERANGE output in media playlists, and real
  `CAN-SKIP-DATERANGES` / `RECENTLY-REMOVED-DATERANGES` handling in delta updates.
- [ ] Evaluate `SAMPLE-AES` (CMAF `cbcs`) encryption with keys from an external key
  service. Moderate effort: encryption boxes in init segments and every fragment,
  including each LL-HLS part, and pattern encryption of video slice data. A plain key URL
  is access control rather than DRM, and player support for clear-key `SAMPLE-AES` is
  unverified. The main value is groundwork for FairPlay/Widevine through a DRM vendor;
  licensing stays out of scope.
- [ ] Read hang `vtt` text renditions: each frame is a self-contained WebVTT segment
  with its own cue timing, so it needs a small cue parser and a rule for which clock
  those timings are on. `utf8` cues are already ingested. Start once a real publisher
  sends `vtt`; the IETF streaming format may yet change how cues are carried.
- [ ] Map a hang text rendition's `role = "caption"` to `CHARACTERISTICS` with the
  `public.accessibility.transcribes-spoken-dialog` and `describes-music-and-sound`
  values, so players can tell SDH captions from subtitles. The playlist writer has no
  `CHARACTERISTICS` support yet.

## Completed baseline

These items no longer belong in the implementation backlog:

- Strict/permissive input policy through `strict = true|false`.
- Backward timestamp rejection, rounding tolerance, and bounded forward GAP handling
  for audio and supported fixed-cadence video.
- Exact missing-media intervals through packaging and delivery, without audio synthesis
  or video frame-hold compensation. No automatic discontinuity accompanies each GAP.
- Degraded/recovered notifications, compensation totals, and structured timestamp failures.
- Video end-to-end tests, blocking-reload tests, reconnect regressions, and Apple
  validation fixtures. These complement, but do not replace, sustained-load validation.
- Strict clippy passes for all Rushls targets without new lint suppressions.
- Metadata-only `segment.ready` hooks after completed media commits. GAP entries do
  not emit ready events. Binary payload hooks remain outside the implemented scope.

See [input handling](docs/input-handling.md), [audio GAP validation](docs/development/gap-validation/audio.md),
and [hook configuration](docs/configuration.md#hooks) for the current behavior.
