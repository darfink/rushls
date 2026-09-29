# Player regression and remaining loading failures

Local evidence paths below are relative to the repository root and are not included in a clone.
See [validation artifacts](../artifacts.md) for storage and reproduction guidance.

Date: 2026-09-20. This follows the [ENDLIST trace](endlist-trace.md).

## Candidate and regression

An isolated hls.js worktree uses tag v1.7.3 on branch `el-codex/endlist-pending-parts`.
The existing hls.js checkout and its unrelated changes remain untouched.
The candidate keeps the previous parent eligible after ENDLIST while non-GAP parts remain unloaded.

Two regressions replay live-to-ENDLIST selection with main and audio fragments.
Both fail before the change and pass afterward.
They also cover completed parts, a GAP-only remainder, and disabled part loading.
All 1,204 unit tests pass with the candidate. Changed-file ESLint and the browser bundle build pass.
Source type checking with `--skipLibCheck` passes.
Full type checking reports a chokidar/Node `FSWatcher` declaration mismatch in the reused dependency installation.

The patch (`target/rushls-validation/endlist-fix-2026-09-20/hlsjs-v1.7.3.patch`) and
draft description (`target/rushls-validation/endlist-fix-2026-09-20/upstream-description.md`) are saved for review.
They were not submitted upstream. The candidate is partial, not a validated playback fix.

## Live validation

The built candidate used the normal probe, with no runtime selector override.
All runs pinned video level 0 and requested no rendition switches.
The official-player controls used hls.js 1.7.3. The patched runs used the locally built full bundle.

| Case | Result | Final media time | Final video part | MediaSource |
| --- | --- | --- | --- | --- |
| Official player, uninterrupted control 1 | Failed | 23.955 s | Marked loaded | Open |
| Official player, uninterrupted control 2 | Failed | 23.853 s | Unloaded | Open |
| Candidate, audio loss | Failed | 23.853 s | Unloaded | Open |
| Candidate, uninterrupted control | Failed | 23.179 s | Marked loaded | Ended |

In the candidate audio-loss run, fragment selection returned the parent rather than null.
The final video part still did not load. Thus, fixing selection alone is insufficient for this case.
A repeat trace identified a downstream blocker: fragment-tracker state was `OK` while the final part remained unloaded.
The video loader starts fragments in `NOT_LOADED` or `PARTIAL` state, so parent selection alone does not start that missing part.
The repeat stopped at 23.852 seconds. This needs separate regression coverage and a loading-state correction.

Control 1 marked all final video parts loaded, but the last video SourceBuffer range ended at 22.2 seconds.
The media clock later stopped at 23.955 seconds. Loaded status did not establish buffered media availability.
The candidate control reproduced the separate failure with ended MediaSource and complete final buffer endpoints.
Neither control contained input GAPs.

The probe now records MediaSource duration and browser playback-quality counters alongside individual SourceBuffer ranges.
These diagnostics distinguish loading, buffering, and playback progress. They do not prove decoded picture correctness or perceptual A/V synchronization.

## Scope and evidence

The case index (`target/rushls-validation/endlist-fix-2026-09-20/results.json`) and compressed reports retain the observed failures.
The archive also retains unit-test logs, build output, the candidate patch, and probe snapshots.
The Rust origins completed successfully. Rushls packaging did not change.

The [tracker correction](tracker-fix.md) adds coverage for buffer padding and parent growth, with new live validation.
The fully buffered, ended-MediaSource stall remains a separate playback problem.
Do not describe the candidate as a complete fix or close the codec/player matrix on the basis of its unit tests.
