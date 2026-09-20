# Timestamp failures

Rushls rejects invalid audio timing and excessive video timestamp steps before packaging.
Audio gaps use [bounded interval handling](audio-recovery.md) in permissive mode. Other invalid timing remains fatal; Rushls does not reset timestamps.

## Configuration

Use [input modes](input-modes.md) to select strict rejection or bounded permissive compensation.
The previous public timestamp-step and audio-recovery options are removed.
Internal video limits accommodate a known slow nominal or declared cadence. Packaging limits still apply.

Checks run per track during pre-roll and live processing. Pacing does not enforce this limit.
For reordered video, PTS movement does not establish a decode-clock jump.
Audio timestamps within the existing rounding tolerance snap to the sample clock.
Larger audio gaps use the selected input mode. Overlaps remain fatal, regardless of the video limit.
Initial track offsets and declared encoder priming remain intact.

## Host reporting

Subscribe to `session.ended` on the existing webhook connection to receive timestamp failures.
The event retains `outcome = "failed"` and the human-readable `diagnostic`.
A timestamp failure also includes `timestamp_issue`. Other failures omit this object.
Hook retry and delivery monitoring retain their existing behavior.

| Field | Meaning |
| --- | --- |
| `code` | Stable issue code from the table below |
| `track_id`, `media_kind`, `codec` | Affected track and encoding |
| `field` | `pts`, `dts`, or `duration` |
| `reference`, `actual` | Exact values as decimal strings |
| `timebase` | Tick duration as integer `numerator` and `denominator` |
| `tolerance_ticks` | Applicable rounding tolerance as a decimal string, or null |
| `maximum_ns` | Applicable limit in nanoseconds as a decimal string, or null |
| `missing_ticks` | Exact missing audio duration as a decimal string, or null |
| `cadence` | Video declaration source, interval, applicable scope, or unavailable-validation reason |
| `recovery_rejection` | Why audio repair failed; omitted for unrelated timing failures |

Values use the supplied timebase. Video timestamp steps use the source clock.
Audio failures use the normalized sample clock. Video duration failures use the source clock when supplied, otherwise the normalized clock.
Cadence violations use an exact common clock for expected and actual presentation positions.
For audio, `reference` is the expected next timestamp or accepted PTS.
For video order and jump failures, `reference` is the previous timestamp.
For duration failures, `reference` is zero and `actual` is the duration.

| Code | Meaning |
| --- | --- |
| `video_cadence_violation` | Presentation timing violates the declared cadence or exceeds its compensation budget |
| `video_cadence_unavailable` | Strict mode cannot validate the explicit cadence declaration |
| `video_cadence_conflict` | Explicit cadence declarations contradict each other |
| `audio_gap` | Audio PTS exceeds the expected end; supplied DTS agrees or is absent |
| `timestamp_overlap` | Audio PTS precedes the expected end beyond tolerance |
| `audio_dts_mismatch` | Audio DTS disagrees with the accepted PTS or candidate gap PTS |
| `initial_timestamp_mismatch` | First audio timestamps disagree with the declared start or priming |
| `video_timestamp_order` | Duplicate or backward DTS, or non-reordered PTS |
| `video_timestamp_jump` | Video timestamp step exceeds policy |
| `video_duration_limit` | Derived or fallback video duration exceeds policy |
| `video_dts_mismatch` | Explicit video DTS disagrees with the normalized decode clock |

One failed session increments `rushls_timestamp_rejections_total{code,media_kind}` once.
Timestamp details also appear in structured failure logs. Metrics do not contain track IDs or timestamp labels.
Previously published media remains subject to the existing retention and reconnect policy.

Audio recovery budgets and degraded-state notifications are documented in [Audio gap recovery](audio-recovery.md).
