# Input modes

Rushls offers two input modes. Permissive mode is the default.

```toml
[accept]
strict = false # true rejects timing violations
```

Use `--accept-strict=true` or `RUSHLS_ACCEPT_STRICT=true` to override the file.
Named `[accept.policy.NAME]` tables accept `strict`. Each named policy defaults independently to strict = false.
Resolution translates the boolean to the internal timing policy.

Recovery limits are internal.

## Timing contracts

Strict mode rejects real audio holes and violations of explicit fixed video cadence.
It preserves timestamp rounding tolerance, initial offsets, encoder priming, and valid presentation reordering.
A nominal video rate alone does not establish fixed cadence. Both modes accept variable timing without a fixed declaration.

Rushls reads fixed declarations from H.264 SPS VUI, HEVC VPS/SPS HRD, and AV1 sequence timing.
Publisher-supplied HEVC configuration can also declare constant rate. Synthesized MPEG-TS configuration cannot supply that evidence.
An unset fixed-rate flag means unspecified timing, not proven variable timing.

Validation requires an unambiguous progressive picture interval and supported picture/layer scope.
Strict mode rejects fixed declarations that Rushls cannot validate. Permissive mode reports unavailable validation once and retains basic timing checks.
Contradictory explicit declarations fail in both modes.

## Permissive compensation

Permissive mode represents missing audio with explicit GAPs.
It also emits video GAPs for declared fixed, progressive H.264 cadence without presentation reordering.
Other video mappings reject cadence holes with `unsupported_configuration`.
Early pictures beyond rounding tolerance remain fatal. Neither mode falls back to extending frames or synthesizing audio.
An accepted late picture retains its timestamp and starts the next cadence expectation.
This includes delays that are not whole frame intervals.

Each track has these internal limits:

| Limit | Audio | Declared fixed video |
| --- | --- | --- |
| Individual compensation | 500 ms hole | 500 ms excess over nominal interval |
| Rolling compensation | 1 second | 1 second excess |
| Rolling count | 10 holes | 10 late intervals |
| Rolling window | 60 seconds | 60 seconds |
| Clean media before recovery | 30 seconds of real audio | 30 seconds of conforming presentation timing |

Budgets use each track's media clock. Equality is permitted. Entries expire at age 60 seconds.
A new publishing session resets budgets. Recovery notifications do not reset them.

Without verifiable fixed cadence, the video timestamp-step ceiling is 500 ms.
A known declared or nominal interval raises this ceiling when necessary, with an allowance for timestamp quantization.
Packaging limits still apply independently. Ordinary duration derivation for variable timing remains unchanged.

Rushls does not duplicate video frames, synthesize audio, resample, reset timestamps, or generate discontinuities.
Unsupported gap handling and exhausted budgets terminate publication.

## Host notifications and metrics

Subscribe to `session.degraded`, `session.recovered`, and `session.ended` through the existing hooks.
Logs warn when an episode starts. Logs report recovery at informational level.
Every accepted violation contributes to metrics and episode totals.

Transition events contain a `compensation` object. Final events contain a `compensation` array for affected tracks.
Each object identifies `media_kind`, `track_id`, `codec`, `method`, rational `timebase`, exact durations, and episode/session totals.
Methods are `gap` and `unverified_cadence`. Gaps report zero replacement ticks.
`episode_holes` and `total_holes` count audio repairs or accepted late video intervals. These counts do not prove packet loss.
Integer tick values use decimal strings. Video accounting uses a common rational clock to preserve fractional excess durations.

Cadence details include declaration source, interval, supported scope, or the reason validation is unavailable.
Unavailable validation never becomes recovered merely because timestamps appear regular.
Publication ending also does not imply recovery.
Notices describe normalization; later packaging failures can prevent publication of compensated media.

`rushls_video_timestamp_step_seconds` measures usable source intervals, including nominal and unknown cadence.
These observations do not assert packet loss.
`rushls_video_cadence_violations_total` counts declared-cadence violations.
`rushls_video_compensation_seconds_total` counts accepted excess duration.
Labels use bounded codec/method values and omit track IDs.

Fatal timing failures retain `outcome=failed` and a structured `timestamp_issue` in `session.ended`.
Declared-cadence failures include expected/actual timing and declaration provenance.
See [timestamp failures](timestamp-failures.md) for retention and rejection reporting.

## Playback compatibility

GAP handling is active in production; no internal activation switch remains.
Playlists omit blanket segment-independence claims because resumed audio or video can depend on earlier media.
Part independence flags still describe individual parts.
Omitting the playlist tag departs from Apple’s video-playlist authoring requirement. The validator report retains this explicit exception.

GAP signaling does not restore missing decoder references or guarantee seamless playback.
Safari video-only continuation and some hls.js startup or completion cases remain unresolved.
See [video validation](video-gap-validation.md) and [audio validation](audio-gap-validation.md) for measured results.
HEVC, AV1, and reordered-video GAP recovery remain unsupported.
Use strict mode when accepting missing input is inappropriate.

See [GAP regressions](gap-regressions.md) for packaging and browser checks.
