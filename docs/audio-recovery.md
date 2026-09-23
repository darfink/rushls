# Audio gaps

Rushls represents accepted missing audio as explicit intervals. It does not synthesize AAC silence or Opus concealment packets.
The interval mechanism supports any media kind. Audio and supported H.264 video normalization produce intervals.

## Implementation status

Permissive mode accepts bounded audio gaps through the production pipeline.
Strict mode reports `disabled`. Neither mode falls back to synthesis.
Tests use the same normalizer as production; no playback activation flag remains.

Player compatibility remains limited by the results in [audio validation](audio-gap-validation.md) and [video validation](video-gap-validation.md).
Activation does not establish seamless playback or CMAF conformance for damaged streams.

## Input policy

```toml
[accept]
strict = false
```

Strict mode requires continuous audio within existing timestamp tolerance.
Permissive mode permits bounded gaps.
Initial offsets, encoder priming, and timestamp rounding remain valid.
Overlaps, contradictory timestamps, overflow, and absent timing evidence never become recoverable gaps.

Each track has independent internal limits:

| Limit | Value |
| --- | --- |
| Individual hole | 500 ms |
| Missing duration per rolling window | 1 second |
| Holes per rolling window | 10 |
| Rolling window | 60 seconds |
| Continuous real audio before recovery | 30 seconds |

Budgets count exact missing duration once per input hole, regardless of the number of resulting parts or segments.
Equality is permitted. Each hole enters the window at its ending timestamp and expires at age 60 seconds.
A new publishing session resets the budget. A recovered notification does not.

Gaps need not contain a whole number of codec frames. The next real packet retains its accepted timestamp.
No timestamps are rebased, and priming is not reapplied.
Continuous-publication codec support remains unchanged, including supported AAC-LC, HE-AAC, HE-AAC v2, and Opus configurations. FLAC over Enhanced RTMP also uses this policy; see [FLAC support](flac.md).

## Segment model

Rushls closes available audio before the missing interval, publishes gap-only parents, then starts another audio segment.
Video keeps its existing boundaries. Audio returns to the existing boundary schedule without shifting the global grid.
Temporary audio/video segment-boundary differences are intentional and require player validation.

Gap segments cover only missing time. Their parts have positive durations no longer than the frozen part target.
Available and missing media never share a parent. Previously published parts remain available.
A single missing AAC frame can produce a roughly 21 ms gap segment.

The playlist uses `EXT-X-GAP` for unavailable parents and `GAP=YES` for unavailable parts.
Their URIs return 404. No empty MP4 files are created.
Targets, initialization, presentation timestamps, and discontinuity sequence remain unchanged.
New renditions start at media sequence 1 to avoid an hls.js sequence-zero part-loading bug.

Audio parts after a gap do not claim independence.
When a permissive policy is configured, master and media playlists omit the global independence guarantee.
Recordings contain the available shortened segments and no files for missing intervals.

Players may conceal, skip, or interrupt playback briefly. GAP signaling does not promise silence or restore decoder dependencies.

## Host notifications

Subscribe to `session.degraded`, `session.recovered`, and `session.ended` through the existing webhook connection.

- `session.degraded` reports the first accepted hole in an episode for an audio track.
- Further holes update totals without repeating the degraded transition.
- `session.recovered` follows 30 seconds of continuous real audio. Publication ending never implies recovery.
- `session.ended` includes final totals and any open episodes in its `compensation` array.

The compensation method is `gap`. Objects identify media kind, track, codec, and rational timebase.
`missing_ticks` is the latest hole's duration. `replacement_ticks` is zero.
Episode and session totals count missing ticks and holes. Tick values remain decimal strings.

Notices describe normalization, not proof of publication. They survive later failures in the same batch.
Episode entry logs at warning level; recovery logs at informational level.
The existing compensation counters count every hole and its exact duration with bounded codec/method labels.

Rejected gaps retain `timestamp_issue.code = "audio_gap"` and a typed rejection reason:
`disabled`, `unrepresentable_duration`, `maximum_hole`, `maximum_compensation`, or `maximum_holes`.
Failure releases the publishing lease and preserves earlier media under existing retention rules.

## Validation

See [Audio GAP validation](audio-gap-validation.md) for measured results and the remaining release checks.

Decoder checks generate real audio, remove packets, and package the remaining audio through Rushls with 200 ms parts.
They check decoded continuation rather than treating successful muxing as successful recovery.

```sh
RUSHLS_GAP_FIXTURES=/tmp/rushls-gap-fixtures cargo test -p rushls --lib gapped_audio_decodes -- --ignored
RUSHLS_GAP_FIXTURES=/tmp/rushls-gap-fixtures cargo test -p rushls --lib he_aac_gaps_decode -- --ignored
```

The HE-AAC check requires FFmpeg's macOS AudioToolbox encoder.
Exported playlists and media come from the production muxer, store, and playlist projection.
`index.m3u8` includes parts; `full.m3u8` omits LL-HLS tags for full-segment checks.

The browser probe requires Safari remote automation and a local hls.js bundle from its official distribution.
Start `safaridriver -p 4445`, then run:

```sh
python3 tools/check-gap-playback.py \
  --fixtures /tmp/rushls-gap-fixtures --hls-js /path/to/hls.min.js \
  --output /tmp/gap-playback.json
```

Use `--playlist full.m3u8` for full-segment checks.
The probe checks playback progress, continuation, and seeking after the gap.
These completed-playlist checks do not replace live LL-HLS, A/V synchronization, or alternate-rendition tests.
