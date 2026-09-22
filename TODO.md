# Rushls TODO

The core media pipeline is implemented within the limits documented here.
Production readiness still requires operational validation and CI/CD.
Future features are not release requirements unless a deployment needs them.

## Release validation and hardening

Complete these checks before a broad production rollout:

- [x] Extend the soak harness with real media downloads, slow consumers, CPU/RSS,
  process allocation counters, latency limits, resource bounds, and cleanup assertions.
- [x] Exercise concurrent reconnects, malformed RTMP connections, abrupt publisher
  loss, recorder write failure/recovery, and DVR pressure alongside healthy streams.
  Record the executed workloads and results in [load validation](docs/load-validation.md).
- [ ] Establish deployment-specific capacity through longer runs on representative
  hardware and media. Include real disk-full/stalled-device and packet-loss scenarios
  where the deployment requires them. Local validation is not a production capacity claim.
- [x] Review ELST/PDT offsets and reconnect fetch grace. Add HTTP regressions for
  real CMAF A/V offsets and priming, part/segment/init fetch deadlines after reconnect,
  and blocking reloads across successor publication and rendition retirement.
- [ ] Complete the supported codec/player matrix for startup, seeking, live playback,
  and rendition switching around GAPs. Record transient freezes and skips separately
  from persistent stalls. Track results in the [player matrix](docs/gap-player-matrix.md)
  and use the [GAP regression checks](docs/gap-regressions.md).
- [ ] Aggregate hook failure logs into unhealthy/recovered transitions and periodic
  totals. Preserve per-event counters. Include a rejecting endpoint under
  `segment.ready` traffic in the load tests.
- [ ] Decide where publisher fairness is enforced: application admission or deployment
  infrastructure. Document and test the chosen protection against one publisher
  exhausting global capacity, including input without pacing.
- [ ] Automate build, tests, formatting, lint checks, and release delivery in CI/CD.
  Keep Apple validation on a suitable macOS runner. Report unavailable tools as
  skipped checks, never as successful validation.

## Known limitations and unresolved reports

These limits remain explicit in the supported scope:

- Safari video-only playback can end prematurely at a video GAP. Tested A/V cases
  continued, sometimes after a freeze until the next IDR. This does not establish
  support for every codec, player, or seek position.
  See [video GAP validation](docs/video-gap-validation.md).
- Permissive video GAP recovery supports progressive H.264 without presentation
  reordering. Unsupported video recovery mappings remain fatal.
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

- [ ] Support coordinated midstream discontinuities and a defined subset of compatible
  codec/configuration changes. Define timestamp-reset semantics before adding recovery.
- [ ] Extend video GAP recovery to additional codecs and presentation mappings with
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

See [input modes](docs/input-modes.md), [audio GAP validation](docs/audio-gap-validation.md),
and [hook configuration](docs/config.md#hook-delivery) for the current behavior.
