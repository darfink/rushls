# Live GAP regression follow-up

Local evidence paths below are relative to the repository root and are not included in a clone.
See [validation artifacts](validation-artifacts.md) for storage and reproduction guidance.

Date: 2026-09-20. These runs used production normalization, publication, storage, and HTTP delivery.
They cover H.264/AAC live playback only. They do not complete startup, seeking, or the wider codec matrix.

## Probe corrections

Every scheduled rendition switch must occur and produce a matching selection event.
Unsupported requests, unchanged selections, and missing events fail the switch check.
Initial events and events after the next request cannot satisfy an earlier request.
The report retains both the planned video target and the actual target selected after ABR decisions.

Twelve Python tests passed, including eight switch-validation regressions.
Python compilation and whitespace checks passed. Production Rust code did not change in this follow-up.

## Environment and results

The host ran macOS 26.6.2, build 25G83.
Native Safari was 26.6.2. Chrome was 153.0.8010.50 with ChromeDriver 153.0.8010.52.
Chrome used the official hls.js 1.7.3 bundle, without player patches, in headless mode.
Safari remained visible and started through WebDriver. No manual activation was necessary.
The Safari and Chrome switching cases ran concurrently. The two Chrome no-switch cases ran separately.

Each case used the same encoded 24-second input, verified by SHA-256.
The fixture has two video variants and two AAC renditions, with 200 ms parts.
The damaged cases used the `combined` loss scenario. Controls retained all input packets.
The playlists contained no added discontinuities.

| Case | Result | Observed switches | Final media time | Final clock plateau |
| --- | --- | --- | --- | --- |
| Safari, GAP, audio switches | Passed | 2/2 | 24.009 s, ended | None |
| Safari, control, audio switches | Passed | 2/2 | 24.048 s, ended | None |
| Chrome, GAP, audio/video switches | Failed | 4/4 | 23.848 s, not ended | 15.56 s |
| Chrome, control, audio/video switches | Failed | 4/4 | 23.178 s, not ended | 13.54 s |
| Chrome, GAP, no switches | Failed | Not requested | 23.957 s, not ended | 14.07 s |
| Chrome, control, no switches | Passed | Not requested | 24.021 s, ended | None |

All six origin tests passed their session and publication assertions.
No case recorded an unsolicited rewind. Successful origin publication does not imply successful playback.
The plateau measures unchanged sampled media time within 10 ms until the probe deadline.
The automated probe allowed 38 seconds after playback activation.

## Transient presentation changes

The Safari GAP case had 1.439 s and 1.401 s between callbacks around two video holes.
Presented timestamps advanced from 5 to 6 seconds and from 13 to 14 seconds, at the next IDRs.
The Safari control also had a 2.083 s callback interval during audio switching.
Its presented timestamp advanced only 40 ms across that interval.

Chrome recorded large callback intervals in both damaged and control cases.
Maximum intervals were approximately 3.1–3.4 seconds. The largest presented timestamp step was 4.32 seconds in the switching GAP case.
Even the successful no-switch control had a 3.92-second presented timestamp step.
These observations must not become a claim of smooth control playback.

Callback intervals measure observed presentation, not every decoded frame or the audible audio position.
The saved summary excludes startup pairs whose earlier media timestamp is at most 100 ms.
It records callback intervals over 500 ms separately from media-time steps and terminal plateaus.
These thresholds summarize observations. They are not new production recovery limits or perceptual quality criteria.

## Interpretation and next isolation

Safari supports continuation in this tested A/V case, including audio switches and transient freezes.
This result does not change the known video-only Safari limitation.

The Chrome switching control also failed, so switching failures cannot be attributed solely to GAPs.
The no-switch pair isolates a completion difference: the control ended, but the damaged stream did not.
One matched pair is evidence for further isolation, not proof of a player or origin defect.

The switching GAP report left the final video part unloaded.
The no-switch GAP report marked all final video parts loaded but remained idle without `ended`.
Thus, the earlier final-part explanation does not cover every failure in this set.
Buffered ranges also contain holes in the uninterrupted Chrome control.
The next investigation must distinguish declared media timing, loaded parts, and appended MSE ranges before assigning a cause.

The [Chrome isolation follow-up](gap-chrome-isolation.md) repeats no-switch cases with audio-only loss and video-only loss with continuous audio.
Retain official-player results separately from diagnostic player changes.
Startup positions, interactive seeks, other audio codecs, and HEVC/AV1 with audio loss remain pending.

## Retained evidence and reproduction

The case index (`target/rushls-validation/gap-live-2026-09-20/results.json`) records source and player hashes, timing observations, and outcomes.
The same directory contains compressed browser reports, origin logs, session events, probe snapshots, and the production source patch.
The patch records the existing uncommitted FLAC work against the base revision in the index.
The early Chrome run that exposed the unchanged-target problem is excluded from this corrected six-case set.
That exploratory report remains under `/tmp/rushls-gap-review-20260920/chrome-gap.json`.

The final switch validator was also applied to every saved report. The stricter event boundary did not change any result.
Raw media is reproducible through the fixture generator. Its expected hash is in the case index.
The archived reports preserve playlists, but do not contain downloadable media objects or a network packet capture.

Start the origin with a fresh ready-file path:

```sh
RUSHLS_GAP_LIVE_READY=/tmp/gap-repeat.ready RUSHLS_GAP_LIVE_VIDEO=combined \
  cargo test -p rushls --lib live_av_gap_browser_origin -- --ignored --nocapture
```

Start the Chrome probe with an official local hls.js bundle:

```sh
python3 tools/check-live-gap-playback.py \
  --ready /tmp/gap-repeat.ready --hls-js /path/to/hls.min.js \
  --output /tmp/gap-repeat.json --switches all
```

For controls, add `RUSHLS_GAP_LIVE_CONTROL=1` to the origin environment.
For no-switch cases, use `--switches none`.
For Safari, use `--browser safari --mode native --webdriver http://127.0.0.1:4445 --switches audio`.
Use separate ready-file names for every run.
