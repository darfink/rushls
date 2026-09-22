# Live part ordering correction

Local evidence paths below are relative to the repository root and are not included in a clone.
See [validation artifacts](validation-artifacts.md) for storage and reproduction guidance.

Date: 2026-09-20. Follow-up to the [append-order investigation](gap-append-order.md).

The local hls.js candidate now preserves pending parts across live segment boundaries and ENDLIST.
The uninterrupted control completes with continuous buffers, all 600 video frames, and no duplicate or backward appends on either track.
This is a player correction. Rushls packaging and GAP policy are unchanged.

## Changes

- Prefer an unloaded, non-GAP part at the requested buffer edge before advancing to the next parent.
- Disable segment lookup tolerance while selecting parts. That tolerance can exceed a complete part's duration.
- Keep part selection active after ENDLIST removes the fragment hint.
- Preserve loaded-part markers when ENDLIST updates the same tracked parent.
- Ignore stale unloaded-part markers behind the buffer edge when overriding normal loop protection.

The buffer-edge comparison allows 1.5 microseconds for browser quantization and accumulated playlist rounding.
It does not permit skipping an actual media part.
This matches the existing fragment finder's buffer-rounding allowance.

The regression tests cover main and audio selection, GAP exclusion, completed tails, rounding, and ENDLIST without a fragment hint.
Another regression prevents ENDLIST from removing the same tracked parent and clearing its loaded-part markers.
The new main/audio boundary tests failed before the correction.

## Verification

All 1,211 hls.js unit tests pass.
Changed-file ESLint, Prettier, `git diff --check`, and TypeScript with `--skipLibCheck` pass.
The previously recorded full TypeScript dependency limitation remains separate.
The Rushls browser probe's 13 tests also pass.

Fresh Chrome live runs use the normal player, fixed video level 0, and no requested rendition switches.
The case index (`target/rushls-validation/live-order-fix-2026-09-20/results.json`) records exact buffers, player hashes, frame counts, and append counts.
Browser reports retain the complete append capture and playlist trace.

A second uninterrupted run disables append capture and fragment-selection tracing.
It also completes with continuous buffers, all 600 video frames, and zero dropped or corrupted frames.
Both uninterrupted runs reach 24.021333 seconds.

The audio-GAP run completes at 24.021333 seconds with continuous video buffers and all 600 video samples appended once, in order.
Playback quality reports 587 video frames as the player skips across missing audio.
The captured audio buffer holes match the two intended missing intervals.
Two repeated audio appends and one backward audio append remain near the gaps. They do not cause a persistent stall in this run.
Those bounded audio repeats remain an open player finding. This result is not a claim of perfect GAP recovery.

The single-video-GAP case also completes at 24.021333 seconds.
It appends and presents the 599 surviving video pictures with no duplicate or backward video appends.
Chrome reports continuous video buffer ranges across this small gap. That does not mean the origin generated a replacement picture.
One repeated audio append remains in that run, without a persistent stall.
Both GAP cases report zero dropped or corrupted video frames through the playback-quality API.
This API result does not establish perceptual concealment quality.

Intermediate candidates either reloaded completed audio segments repeatedly or replayed the final video parent.
They are superseded. The final byte capture is necessary evidence: an ended event alone had concealed both defects.

## Scope and review

The player investigation used a separate hls.js worktree on branch `el-codex/endlist-pending-parts`.
The archived patch includes the earlier selector, tracker, and loader corrections against hls.js v1.7.3.
No patch was submitted upstream, and no Rushls player dependency was changed.

Next work is to isolate the remaining audio repeats, then validate rendition switching with the corrected player.
Safari limitations and the wider codec/player matrix remain separate.
