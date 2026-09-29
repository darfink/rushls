# Chrome GAP isolation

Local evidence paths below are relative to the repository root and are not included in a clone.
See [validation artifacts](../artifacts.md) for storage and reproduction guidance.

Date: 2026-09-20. This follow-up separates audio loss from video loss in the live H.264/AAC fixture.
It follows the [six-case live check](live-follow-up.md).

## Method

Five cases ran sequentially with no requested rendition switches.
The host, browser, driver, official hls.js 1.7.3 bundle, and encoded input match the preceding investigation.
Chrome ran headless. The input hash was identical in all five cases.
Automatic player decisions remained enabled. “No switches” means no scheduled probe requests, not proof of a fixed variant throughout playback.

Audio-loss cases omitted `RUSHLS_GAP_LIVE_VIDEO`.
Video-loss cases used `RUSHLS_GAP_LIVE_VIDEO=repeated` and retained continuous audio.
The control used `RUSHLS_GAP_LIVE_CONTROL=1` and retained every packet.
Every probe used `--switches none`.

The final playlists confirmed the intended loss patterns:

- Audio cases: three and two GAP parents in the audio renditions, with no video GAPs.
- Video cases: three GAP parents per video rendition, with no audio GAPs.
- Control: no GAP parents.

All five origin tests passed. No production behavior or player bundle changed.
The probe now captures each SourceBuffer separately, including buffered ranges, update state, timestamp offset, and append windows.
It also records MediaSource state. The first audio run preceded this diagnostic addition.

## Results

| Case | Result | Final media time | Final clock plateau | Final video part |
| --- | --- | --- | --- | --- |
| Audio loss | Failed | 23.837 s | 13.19 s | Unloaded |
| Audio loss, repeat | Failed | 23.848 s | 13.79 s | Unloaded |
| Video loss with continuous audio | Passed | 24.021 s | None | Loaded |
| Video loss with continuous audio, repeat | Failed | 23.845 s | 14.83 s | Unloaded |
| Uninterrupted control | Passed | 24.021 s | None | Loaded |

Failed cases did not emit `ended`. They crossed the earlier missing intervals and stopped near the end of publication.
No run recorded an unsolicited rewind.

The audio repeat left MediaSource open, with video buffered through 23.8 seconds and audio through 23.9147 seconds.
Neither SourceBuffer was updating at the final snapshot. Both timestamp offsets remained zero.
The available final video part covered 23.8–24.0 seconds, but the player marked it unloaded.

The first video-loss run completed with both SourceBuffers continuous through their respective endpoints.
Its largest post-start callback interval was 233 ms, and its largest presented timestamp step was 240 ms.
The repeat failed, so that single successful run does not establish reliable video-GAP completion.

Other runs had callback intervals of approximately 2.8–3.1 seconds.
The successful control also had a 3.92-second presented timestamp step and holes in its individual SourceBuffer ranges.
Thus, completion alone does not establish smooth playback, and not every buffered hole comes from an input GAP.
Perceptual A/V synchronization and decoded picture quality remain unmeasured.

## Conclusion and next check

Audio loss reproduced the terminal failure twice. Video loss produced one pass and one failure.
The initial hypothesis that only audio loss causes the problem is not supported by the repeat.
The shared symptom is incomplete final-part loading after a live stream ends.
This is distinct from failure to resume at the earlier GAP itself.

These results do not prove whether the remaining defect is in the origin, player, or their timing interaction.
The [ENDLIST trace](endlist-trace.md) captures that transition and the player's part-selection decision.
Use a fixed video variant for that diagnostic, and retain the ordinary-player controls.
Compare final-part resource availability and timestamps with requested, loaded, and appended ranges before changing production packaging.

## Evidence and reproduction

The case index (`target/rushls-validation/gap-isolation-2026-09-20/results.json`) contains exact measurements, input hashes, GAP counts, and final SourceBuffer states.
Its directory retains compressed browser reports, session events, origin logs, and probe snapshots.
The production source patch is unchanged from the preceding six-case archive.
The first audio report has no per-track buffer snapshot. Its repeat supplies that evidence.

Use the [live reproduction commands](live-follow-up.md#retained-evidence-and-reproduction) with the scenario environment described here.
Use a fresh ready-file path for every run. No Safari tests ran in this isolation set.

All twelve playback-tool tests, Python compilation, documentation links, and whitespace checks passed after the diagnostic addition.
The broader codec/player matrix remains open.
