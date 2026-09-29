# Final-part selection at ENDLIST

Local evidence paths below are relative to the repository root and are not included in a clone.
See [validation artifacts](../artifacts.md) for storage and reproduction guidance.

Date: 2026-09-20. This investigation follows the [Chrome isolation cases](chrome-isolation.md).
It identifies one player selection failure. It does not resolve every recorded playback stall.

## Captured failure

The official hls.js 1.7.3 run used video level 0 with no requested switches.
All recorded video selection events stayed on that level. Chrome, the fixture, and the host match the preceding investigation.
The input contained audio loss and uninterrupted video.

The final video playlist response arrived at browser time 21,637.7 ms.
It contained `EXT-X-ENDLIST` and part 9 of media sequence 12, covering 23.8–24.0 seconds.
At 21,638.2 ms, the fragment selector recorded:

| State | Value |
| --- | --- |
| Buffer endpoint | 23.8 seconds |
| Playlist endpoint | 24.0 seconds |
| Playlist live | False |
| Part loading | True |
| Previously selected parent | 12 |
| Final part | Parent 12, index 9, unloaded, not a GAP |
| Selected fragment | Null |

The selector continued to return null. Playback stopped at 23.85042 seconds without `ended`.
The player did not request the selected rendition's final video part.
An independent HTTP fetch returned 200 for that part and the last parts of all other renditions.
Both final video parts had the same byte hashes as their uninterrupted controls.

This establishes that the final video resource was present, advertised, and unchanged by audio loss.
It does not establish conformance of every other part or playlist field.

## Selection rule

In [hls.js v1.7.3 `getFragmentAtPosition`](https://github.com/video-dev/hls.js/blob/v1.7.3/src/controller/base-stream-controller.ts#L1944),
the selector advances past a fragment equal to the previous fragment when the playlist is no longer live.
For the last parent, there is no next fragment, so the method returns null.
That branch does not preserve the parent merely because some of its non-GAP parts remain unloaded.
The captured state satisfies this branch even though the final part is available.

The same method serves audio and video controllers.
The missing tail can therefore depend on which controller reaches the transition with unfinished parts.
This explains the captured selection failure without assuming that GAPs require discontinuities.

## Diagnostic comparison

A temporary probe override retained the previous parent when the original selector returned null and non-GAP parts remained unloaded.
The override changed no origin code, media, timestamps, or playlists.
It is an investigation aid, not a shipped player patch or a production workaround.

| Run | Result | Diagnostic intervention |
| --- | --- | --- |
| Official player, audio loss | Stopped at 23.850 s | None |
| Official player, control | Completed at 24.021 s | None |
| Diagnostic, audio loss | Completed at 24.021 s | Retained audio parent 16 for pending parts |
| Diagnostic, control | Stopped at 23.182 s | Never activated |
| Diagnostic, audio loss repeat | Completed at 24.021 s | Never activated |

The successful intervention supports the selection diagnosis.
The repeat needed no intervention, so it is not a second proof of the override's effect.
Completion is sensitive to the timing of part loading relative to the final playlist response.

The failed diagnostic control had all final video parts loaded and MediaSource already ended.
Both SourceBuffers extended through their endpoints, but the media element did not finish playback.
That failure has a different signature and remains unresolved.
It must not be attributed to the skipped-final-part branch.

## Evidence and next work

The case index (`target/rushls-validation/endlist-trace-2026-09-20/results.json`) records all five runs.
The directory retains compressed raw reports, exact playlist responses, origin logs, session events, final-part bytes, and resource hashes.
It also retains the diagnostic probe and the inspected player source.
The production source snapshot is unchanged from the preceding live validation archive.
No trace exceeded the 6,000-entry limit. All origin tests passed.

The reusable probe now accepts `--fixed-level 0 --switches none --trace-endlist`.
The diagnostic override exists only in the archived experiment, not in the reusable probe.
Thirteen playback-tool tests, Python compilation, JavaScript syntax, documentation links, and whitespace checks passed.

The [candidate and live follow-up](player-fix.md) add this regression and identify a second tracker-state blocker.

The next player regression should replay the final live playlist followed by ENDLIST while the last parent still has unloaded parts.
It must require the selector to retain that parent for audio and video, then stop after all parts load.
The separate stall with complete buffers and ended MediaSource needs its own investigation.
Production GAP packaging remains unchanged. The codec/player validation TODO remains open.
