# Pending-part tracking and loading correction

Local evidence paths below are relative to the repository root and are not included in a clone.
See [validation artifacts](validation-artifacts.md) for storage and reproduction guidance.

Date: 2026-09-20. This extends the [selector candidate](gap-player-fix.md).
The changes remain in the isolated hls.js worktree. Rushls packaging is unchanged.

## Changes and regression coverage

The tracker uses a 200 ms allowance around buffered ranges.
That allowance can hide an entire missing LL-HLS part and mark its parent complete.
A playlist refresh can also extend a parent after the tracker records it as complete.

The candidate now:

- Requires actual tail coverage before buffer padding can mark an unfinished part-based parent complete.
- Preserves completion after a late callback for an earlier part when the parent's tail is already buffered.
- Keeps later, disconnected buffered segments from satisfying the missing tail.
- Retains the endpoint delivered by parts, so later parent growth can invalidate cached completion.
- Lets audio and video loaders honor explicit unloaded non-GAP parts despite a cached complete-parent state.
- Prevents loop-load suppression from overriding that pending-part evidence.
- Preserves the append-in-progress guard and skips GAP-only remainders.

Tracker corrections alone did not fix every live case.
The loaders also needed the explicit part-list check.
The regression suite covers buffer padding, parent growth, late callbacks, disconnected ranges, loop prevention, and loader state guards.
Both tracker regressions failed before their respective corrections.

All 1,208 hls.js unit tests pass. Changed-file ESLint, formatting, and source type checks with `--skipLibCheck` pass.
The full type-check dependency limitation remains the previously recorded chokidar/Node declaration mismatch.

## Live results

All runs used the normal probe, pinned video level 0, and no requested rendition switches.
There was no runtime selector override.
The case index (`target/rushls-validation/tracker-fix-2026-09-20/results.json`) retains exploratory builds as well as the final loading-path comparison.
Player hashes distinguish those builds.

The final loading-path comparison produced:

| Case | Final-part result | Playback result |
| --- | --- | --- |
| Audio GAPs (`load-audio`) | All final parts loaded, MediaSource ended | Completed at 24.021 seconds |
| Uninterrupted (`load-control`) | All final parts loaded, MediaSource ended | Stalled at 23.164 seconds |

All Rust origin tests completed successfully.
The earlier tracker-only builds still left unloaded tails in some runs. They are not successful validation of the combined fix.
The final TypeScript narrowing guard excludes initialization fragments from the pending-part helper. It does not change the tested media-fragment path.

The final control reproduces the separate playback stall after buffering and MediaSource completion.
Do not interpret the pending-part correction as a fix for that stall, or as completed codec/player validation.
The [MSE comparison](gap-mse-comparison.md) investigates media-element progress with complete final buffers and ended MediaSource.

## Review artifacts

The combined patch (`target/rushls-validation/tracker-fix-2026-09-20/hlsjs-v1.7.3.patch`) includes the selector, tracker, loader, and regression changes against v1.7.3.
The archive retains browser reports, player hashes, session events, test logs, and probe snapshots.
The player investigation used a separate hls.js worktree on branch `el-codex/endlist-pending-parts`.
Nothing was submitted upstream or added to the Rushls player distribution.
