# Input handling

Encoders and networks do not always deliver clean timestamps. This page describes what Rushls accepts,
what it does with a hole in the input, and how a rejected publication is reported.

## Strict and permissive mode

```toml
[publish]
strict = true   # the default; false serves bounded holes as EXT-X-GAP
```

Override it with `--publish-strict=false` or `RUSHLS_PUBLISH_STRICT=false`.
Each `[publish.profile.NAME]` table has its own `strict`, which also defaults to `true`.

**Strict mode** ends the publication on a real hole in the audio, a violation of a declared fixed video cadence,
or a segment that would have to start on a dependent (non-keyframe) picture.

**Permissive mode** serves a bounded hole as missing media instead: the playlist marks it with `EXT-X-GAP`
(or `GAP=YES` on a part), and playback continues after it. Rushls never invents media to fill the hole.
It does not duplicate frames, synthesize silence, resample, reset timestamps, or insert discontinuities.

Both modes accept:

- Timestamp rounding within tolerance, initial offsets between tracks, and declared encoder priming.
- Presentation reordering (B-frames) that is valid.
- Variable frame timing, when the encoder does not declare a fixed cadence.

Neither mode accepts audio that overlaps itself, timestamps that go backwards, or contradictory timing.

## What permissive mode can recover

**Audio.** Missing audio in any supported codec (AAC-LC, HE-AAC, HE-AAC v2, Opus, and FLAC over Enhanced RTMP).
A hole does not need to be a whole number of codec frames. The next real packet keeps its timestamp.

**Video.** Only when the encoder declares a fixed cadence and sends no B-frames:

| Codec | Declaration Rushls reads | Scope | How to request it |
| --- | --- | --- | --- |
| H.264 | SPS VUI fixed frame rate | Progressive frames | libx264 `force-cfr=1` |
| HEVC | VPS/SPS HRD fixed picture rate | Progressive pictures, one temporal layer | libx265 `hrd=1` with VBV settings |
| AV1 | Sequence header timing info with `num_ticks_per_picture` | Single-layer temporal units with one shown frame | SVT-AV1 plus the `av1_metadata=tick_rate=R:num_ticks_per_picture=1` bitstream filter |

A nominal frame rate alone is not a declaration. Without one, a video hole cannot be recovered and ends the publication.
A late picture that is accepted keeps its timestamp, and the cadence continues from it.

### Limits

Each track has fixed recovery limits. Exceeding any of them ends the publication.

| Limit | Audio | Declared fixed video |
| --- | --- | --- |
| Largest single hole | 500 ms | 500 ms beyond the frame interval |
| Total missing time per rolling window | 1 second | 1 second |
| Holes per rolling window | 10 | 10 late intervals |
| Rolling window | 60 seconds | 60 seconds |
| Clean media needed to count as recovered | 30 seconds | 30 seconds |

A new publishing session starts with fresh limits.
Without a declared cadence, a single video timestamp step may not exceed 500 ms, or a known frame interval if that is longer.

### What the viewer sees

Audio before the hole closes into its own segment, the hole is published as gap-only segments and parts,
and audio then returns to the normal segment schedule. Video keeps its boundaries.
Gap resources return 404; no empty media files are created.

When any profile is permissive, playlists omit `EXT-X-INDEPENDENT-SEGMENTS`, because media after a hole can depend on earlier media.
This departs from Apple's authoring requirement for video playlists, and Apple's validator reports it.

GAP signalling does not restore missing decoder references. Players may conceal the hole, skip it, or pause briefly,
and support varies: Safari does not continue video-only playback past a video gap, and some hls.js startup and end-of-stream cases fail.
See [Players](players.md). Use strict mode when a damaged stream should not be served.

Recordings contain the shortened segments around a hole and no file for the missing time.

## Hooks, logs, and metrics

Subscribe to `session.degraded`, `session.recovered`, and `session.ended` through a [hook](admission-and-hooks.md#lifecycle-hooks).

- `session.degraded` reports the first accepted hole on a track. Later holes update totals without a new event.
- `session.recovered` follows 30 seconds of clean media on that track. A publication ending does not count as recovery.
- `session.ended` carries final totals, including episodes still open, in its `compensation` array.

Each `compensation` object names the `media_kind`, `track_id`, `codec`, `method` (`gap`, or `unverified_cadence`
when a cadence declaration could not be validated), a rational `timebase`, and exact durations and hole counts
for the episode and the session. Tick values are decimal strings. Counts describe accepted holes, not proven packet loss.

A degraded episode logs a warning, and recovery logs at info level.
The metrics are `rushls_audio_repairs_total`, `rushls_audio_compensation_seconds_total`, `rushls_video_timestamp_step_seconds`,
`rushls_video_cadence_violations_total`, and `rushls_video_compensation_seconds_total`.
See [Metrics](metrics.md).

## When a publication is rejected

A timing failure ends the publication and is reported in `session.ended` with `outcome = "failed"`,
a human-readable `diagnostic`, and a `timestamp_issue` object:

| Field | Meaning |
| --- | --- |
| `code` | One of the codes below |
| `track_id`, `media_kind`, `codec` | The affected track |
| `field` | `pts`, `dts`, or `duration` |
| `reference`, `actual` | The expected and received values, as decimal strings |
| `timebase` | Tick duration as integer `numerator` and `denominator` |
| `tolerance_ticks` | The rounding tolerance that applied, or null |
| `maximum_ns` | The limit that applied, in nanoseconds, or null |
| `missing_ticks` | The missing audio duration, or null |
| `cadence` | Where the video cadence declaration came from, its interval and scope, or why it could not be validated |
| `recovery_rejection` | Why an audio hole could not be recovered: `disabled` (strict mode), `unrepresentable_duration`, `maximum_hole`, `maximum_compensation`, or `maximum_holes` |

For audio, `reference` is the expected next timestamp. For video order and jump failures, it is the previous timestamp.
For duration failures, `reference` is zero and `actual` is the duration.

| Code | Meaning |
| --- | --- |
| `audio_gap` | Audio starts later than the previous packet ended |
| `timestamp_overlap` | Audio starts before the previous packet ended, beyond tolerance |
| `audio_dts_mismatch` | Audio DTS disagrees with its PTS |
| `initial_timestamp_mismatch` | The first audio timestamps disagree with the declared start or priming |
| `video_timestamp_order` | Duplicate or backward DTS, or backward PTS without reordering |
| `video_timestamp_jump` | A video timestamp step exceeds the limit |
| `video_duration_limit` | A video frame duration exceeds the limit |
| `video_dts_mismatch` | Explicit video DTS disagrees with the reconstructed decode clock |
| `video_cadence_violation` | Timing breaks the declared cadence, or exceeds the recovery limits |
| `video_cadence_unavailable` | Strict mode cannot validate the cadence declaration |
| `video_cadence_conflict` | Two cadence declarations contradict each other |

Each failed session increments `rushls_timestamp_rejections_total{code,media_kind}` once.
Media published before the failure stays available for the retention window, and the publisher can reconnect.

## Irregular keyframes

Keyframes that arrive at uneven intervals are a separate problem from missing media, and both modes handle them the same way.
See [irregular keyframe intervals](configuration.md#irregular-keyframe-intervals).
