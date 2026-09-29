# GAP player validation matrix

Evidence review: 2026-09-20. The historical tables precede the [live regression follow-up](investigations/live-follow-up.md) and [Chrome isolation](investigations/chrome-isolation.md).
The tables summarize recorded experiments, not a completed support certification.
Historical reports use Safari 26.6.2, Chrome 153, and official hls.js 1.7.3 unless stated otherwise.
New runs must record their actual versions.

The latest [local hls.js candidate results](investigations/live-order-fix.md) include two completed uninterrupted controls and completed audio/video GAP runs.
Those results apply to the patched player, not official hls.js. Bounded repeated audio appends remain unresolved.
Raw evidence follows the [local artifact policy](artifacts.md).

## Current implementation and scope

Permissive mode publishes audio GAPs and supported H.264 video GAPs through the production pipeline.
Video recovery requires declared fixed cadence, progressive pictures, and no presentation reordering.
Strict mode rejects these holes. Unsupported video recovery mappings remain fatal.
There is no remaining activation flag to open after player validation.

The initial validation target is native Safari and official hls.js in Chrome, with full segments and live LL-HLS.
Safari with hls.js remains supplementary evidence.
Player support for a codec requires a successful uninterrupted control on the same platform.
An unsupported control is a support exclusion, not a successful GAP test.

HEVC, AV1, and reordered H.264 need tests with audio loss and uninterrupted video.
Their video-loss recovery remains outside the implemented scope.
Safari video-only GAP continuation remains a known failure.

## Recorded audio evidence

“Recorded” means the linked experiment reports a result. It does not mean this review reproduced that result.
“Pending” means the evidence does not establish the requested behavior.
Completed playlists with parts do not establish live LL-HLS behavior.

| Audio configuration | Decoder evidence | Completed-playlist player evidence | Live, seeks, and switching |
| --- | --- | --- | --- |
| AAC-LC, mono/stereo, 44.1/48 kHz | GAP decode recorded | Native Safari starts recorded within the audio matrix | H.264/AAC live subset recorded below. Wider matrix pending |
| HE-AAC, mono/stereo, 44.1/48 kHz | GAP and fresh-decoder checks recorded | Safari starts recorded. Focused full-segment hls.js gap/control starts passed after initial failures | Pending |
| HE-AAC v2, stereo, 44.1/48 kHz | GAP and fresh-decoder checks recorded | Safari starts recorded. Focused 44.1 kHz full-segment hls.js gap/control starts passed after initial failures | Pending |
| Opus, mono/stereo | GAP decode recorded | Safari starts recorded within the audio matrix. No complete Chrome matrix established | Pending |
| FLAC, mono 44.1 kHz/16-bit and stereo 48 kHz/24-bit | Exact PCM and fresh decoding after GAPs recorded | Pending, including uninterrupted browser controls | Pending |

Sources: [audio experiment log](audio.md#initial-completed-playlist-results),
[controlled follow-up](audio.md#controlled-follow-up), and [FLAC validation](flac.md#validation).
The initial audio matrix passed all 24 native Safari checks and 21 of 24 Safari/hls.js checks.
Focused full-segment follow-ups passed 20 comparisons across the three initially failing AAC configurations.
Those results do not establish the full Chrome, seeking, or live matrix.

## Recorded video and A/V evidence

| Case | Native Safari | Chrome with official hls.js | Remaining check |
| --- | --- | --- | --- |
| H.264 video-only, completed playlists, dependent-picture loss | Premature end reproduced on production output | Completion and post-GAP fresh start recorded | Keep Safari exclusion. Repeat Chrome regression |
| H.264/AAC, video loss, completed playlists | Continuation at next IDR recorded. Fresh start inside GAP recorded | Eight startup and two seek checks recorded | Complete matched positions and playlist forms in both players |
| H.264/AAC, overlapping A/V loss, corrected live fixture | Completion and two audio switches recorded | Completion with switches recorded, but uninterrupted control also stalled | Repeat controlled runs on current production output |
| H.264/AAC, integrated detection, live overlapping loss | Completion and audio switches recorded | No-switch completion recorded. Switching stopped before the endpoint | Reproduce switching result with matched controls |
| H.264/AAC, early loss during pre-roll | Player attempt inconclusive | Near-end progress without normal completion | Repeat startup and completion checks |
| HEVC/AV1 or reordered H.264 with audio loss | Pending | Pending | Uninterrupted codec control, then audio GAP cases |

Sources: [video experiments](video.md#playback-observations),
[corrected live fixture](video.md#live-av-follow-up),
[integrated detection](video.md#detection-to-publication-integration),
[production Safari failure](video.md#foreground-retry-failure-reproduced-on-production-output),
and [regression checks](regressions.md#recorded-checks).

Safari sometimes resumed video only at the next IDR, with approximately one second between picture callbacks.
Chrome decoded altered dependent pictures until the next IDR in the video fixture.
Continuation does not establish intact pictures, silence, or perceptual A/V synchronization.

## Evidence limits

- Earlier live audio experiments used a paced adapter that could lose packets during cancellation. They are exploratory evidence only.
- The corrected live fixture removes that defect. Later integrated tests include normalization, budgets, and notices.
- Diagnostic player patches and discontinuity playlists do not establish support for the shipped output with an official player.
- Autoplay denial, hidden-window pauses, and automation failures are inconclusive media results.
- Native Safari audio switching is observable. The harness cannot force native Safari video variant selection through a standard API.
- Existing reports do not establish perceptual A/V synchronization. Frame callbacks alone cannot measure the audible audio position.
- Historical `/tmp` artifact paths are reproduction clues, not durable release evidence. This review did not verify their availability.

## Remaining execution order

1. Strengthen the result checks before expanding the matrix.
   Require every scheduled switch to occur and the selected rendition to change.
   Switch validation now rejects unexecuted, unsupported, unchanged, and unobserved selections, with regression tests.
   Add live seek and startup-position controls, and classify freezes, skips, premature ends, and persistent stalls separately.
2. Repeat H.264/AAC production cases with matched uninterrupted controls in both target players.
   Include audio-only loss, video loss with audio, overlapping loss, and the known video-only Safari failure.
   Run completed-playlist starts and seeks before, inside, and after each GAP, including dependent resumed pictures.
3. Run real live publication with early and repeated holes, boundary-crossing holes, and rendition switches around the holes.
   Exercise one-frame holes, non-integral holes, and holes across several 200 ms parts.
   Record full-segment and LL-HLS results separately.
4. Extend audio coverage to HE-AAC, HE-AAC v2, Opus, and FLAC, with uninterrupted controls first.
   Include audio-only playback and A/V playback, then startup, seeks, and live audio switching.
   Preserve the tested rate, channel count, and decoder configuration in each result.
5. Add HEVC and AV1 with audio GAPs, plus reordered H.264 with audio GAPs.
   Keep video packets intact. Do not imply support for video recovery in these cases.
6. Run the Apple GAP harness and retain both validator and hlsreport output.
   Keep authoring exceptions explicit. A missing tool is a skipped check.
7. Record support decisions and select a small repeatable regression set from the completed cases.
   Keep known failures separate from passing cases.

The existing [regression commands](regressions.md) provide the starting fixtures and browser probes.
The Apple harness already has four loss/control pairs. It checks packaging, not decoder continuation.

## Result record and completion rule

Each run must retain these fields and artifacts:

| Field | Required record |
| --- | --- |
| Identity | Case ID, date, Rushls revision, working-tree patch or clean state, fixture and player-bundle hashes |
| Environment | OS, browser, player version, decoder configuration, playback mode, visibility and activation status |
| Scenario | Loss intervals, control ID, requested start/seek positions, scheduled and observed switches |
| Timing | Startup delay, picture-freeze durations, media-time skips, automatic continuation, final presented position |
| Outcome | Pass, failure, inconclusive, or unsupported control, with observation deadline and reason |
| Diagnostics | Browser JSON, requests where available, playlists, session events, validator output, and reproduction command |
| A/V evidence | Measurement method and result, or explicitly unmeasured |

A transient freeze ends automatically within the observation period. A skip advances past media without presenting it.
A persistent stall fails to resume before the recorded deadline despite available subsequent media.
A premature end occurs before the expected endpoint. It remains a failure even if the player emits `ended`.
Record durations separately from these classifications.

Store the case index and summary in the repository. Retain large media and raw reports in durable artifact storage.
Repeat failures with their controls before assigning a cause. Do not attribute a control failure to missing media.
Verify decoder or audio-output timing separately before claiming that no persistent A/V drift occurs.

The TODO remains open until each intended support cell has reproducible evidence and an explicit support decision.
Known failures can define exclusions, but must not appear as passing support cells.
New player versions require a new result record rather than an assumption that earlier behavior still applies.
