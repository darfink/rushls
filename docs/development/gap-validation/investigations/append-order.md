# Live video append ordering

Local evidence paths below are relative to the repository root and are not included in a clone.
See [validation artifacts](../artifacts.md) for storage and reproduction guidance.

Date: 2026-09-20. Follow-up to the [completed-media comparison](mse-comparison.md).

The captured player appends video parts out of decode-time order and repeats nine parts.
Direct MSE replay reproduces the buffer holes without hls.js.
Sorting the video appends and removing byte-identical duplicates restores all 600 frames.
This identifies an append-sequence defect in the tested local player candidate.
It does not establish which candidate change or upstream path causes the selection error.

The subsequent [live ordering correction](live-order-fix.md) fixes pending-part selection and ENDLIST marker loss in the local candidate.

## First hole

The uninterrupted live fixture uses H.264 Baseline at 25 fps and mono AAC.
The player is the local candidate identified in the preceding comparison, with fixed level 0 and no requested switches.

The first video hole covers 3.8–4.0 seconds:

| Operation | Decode time | Content |
| --- | --- | --- |
| 78 | 3.6 seconds | Five pictures |
| 86 | 4.0 seconds | Five pictures, next parent begins |
| 92 | 3.8 seconds | Five pictures, previous parent ends |
| 94 | 4.2 seconds | Five pictures |

The source clock is 90,000 ticks per second.
The fragment trace shows part 21 requested and appended before part 20.
The late part 20 does not repair the buffered hole. Subsequent dependent pictures also fail to extend the buffer until a later restart.
There are no captured buffer removals, aborts, or append exceptions.

The trace contains 275 appends across both tracks, including initialization.
All 129 video media appends have payloads that match the earlier captured origin at their decode timestamp.
There are 120 unique video media appends. Nine repeats are byte-identical.
The capture retains every append and does not reach its 32 MiB byte limit.

## Controlled replay

The replay keeps both tracks and appends the captured bytes directly to MSE.
It serializes operations before playback. It does not reproduce wall-clock append delays or player seeks.
Audio order and bytes remain unchanged in each comparison.

| Video append sequence | Buffers and playback |
| --- | --- |
| Captured order | Same video holes as live, stalls at 6.769358 seconds |
| Sorted by decode time, duplicates retained | Continuous video only through 22.4 seconds, 560 frames |
| Sorted by decode time, byte-identical duplicates removed | Continuous video through 24 seconds, all 600 frames |

The final case reaches 24.021333 seconds with zero dropped or corrupted frames.
Audio covers 0–24.021333 seconds in all three cases.
The sorted-only case reaches an ended event despite missing video at the tail.
The replay tool now requires complete buffers and 600 frames, rather than treating an ended event alone as success.

The instrumented live run also reaches the end, but retains substantial video holes.
Thus this run reproduces the damaged buffer state, not the exact persistent terminal stall from the earlier run.
The direct replay establishes that append order and duplication can reproduce damaged buffers without live publication timing.
It does not prove that every previously observed stall has the same cause.

## Next correction

Correct player selection before append: finish the previous parent's pending parts before advancing, and prevent repeated media appends.
Add regressions for the live parent transition, not only the final ENDLIST transition.
Then rerun the uninterrupted and GAP live controls with buffer-coverage measurements.
Do not sort arbitrary production media as a workaround. This controlled replay contains no reordering, discontinuities, or configuration changes.
Rushls media generation and GAP policy remain unchanged.

## Evidence and tools

The case index (`target/rushls-validation/append-trace-2026-09-20/results.json`) records buffer coverage and playback quality.
The directory retains original reports, captured append bytes, replay inputs, timing analysis, and tool snapshots.
The analysis scripts retain their original temporary paths as investigation records.

Use `--trace-appends` with `check-live-gap-playback.py` to capture another short hls.js run.
The option records append bytes, offsets, append windows, and completion ranges.
Capture can affect scheduling, so compare outcomes with uninstrumented runs.

To replay an archived input, decompress the chosen `*-trace.json.gz` as `append-trace.json` in a fixture directory:

```sh
RUSHLS_COMPARE_MODES=mse-trace node tools/compare-mse-playback.mjs /path/to/fixture /path/to/hls.js /tmp/replay.json
```

Validation: live capture, three direct-MSE comparisons, 13 existing probe tests, JavaScript syntax checks, and `git diff --check`.
No production Rust changes or upstream submission are included.
