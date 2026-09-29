# Video GAP feasibility: I/P-only H.264

## Current implementation status

Production GAP handling is now active for audio and declared fixed, progressive H.264 without presentation reordering.
The test-only activation paths and blanket playlist independence claims have been removed.
Unsupported video mappings still reject cadence holes. Strict mode still rejects holes.
This activation does not resolve the player failures recorded below or establish CMAF conformance.
The [GAP player matrix](README.md) records current evidence and pending checks.
The experiment log below preserves historical results. Its gate decisions and test counts are not current release status.


## Historical experiment log

Date: 2026-09-20.

This experiment supports exact video GAPs as a limited permissive-mode option.
Chrome with unmodified hls.js continued after a missing reference picture.
Native Safari ended prematurely in video-only tests. With continuous alternate audio, the exact GAP completed without a discontinuity.
See the independent-control follow-up below; the video-only result does not establish a general discontinuity requirement.
At this stage, the experiment did not change production normalization or release gates.

## Fixture and packaging

The ignored `ip_video_gap_playback_fixture` test generates eight seconds of H.264 with ffmpeg/libx264.
The stream has 25 pictures per second, no B-frames, one reference frame, and an IDR every two seconds.
It uses the same encoded packets for the control and damaged cases.
The damaged case replaces normalized picture 126 with a missing interval from 5.04 to 5.08 seconds.
This is a reference P-picture. The next received picture is dependent.

The test uses the real muxer, coordinator, store, and playlist projector.
It bypasses video gap detection to isolate packaging and playback feasibility.
The exported damaged playlist removes the projector's video independence tag.
That export-only adjustment is necessary because production video does not yet emit these gaps.
The output is experimental fMP4 HLS; this test does not establish CMAF or Apple authoring conformance.

The parent durations are 2, 2, 1.04, 0.04 GAP, 0.92, and 2 seconds.
The final normal segment still starts at six seconds.
The gap resource is absent. The resumed segment starts with the original dependent picture at 5.08 seconds.
Rust assertions check sample counts, surviving DTS/PTS, durations, and exact gap coverage.
An external packet comparison also confirms unchanged surviving packet bytes and timing.

## Playback observations

Browsers: Chrome 153 and Safari 26.6.2. Player: official, unmodified hls.js 1.7.3.
All playlists were complete and had ENDLIST before playback began.
A playlist containing parts is not proof of live LL-HLS behavior; players can choose full segments.

| Case | Damaged input | Matching control |
|---|---|---|
| Chrome/hls.js, full segments, start at zero | Completed at 8 seconds; 199 presented frames | Completed; 200 frames |
| Chrome/hls.js, playlist with parts, start at zero | Completed; 199 frames | Completed; 200 frames |
| Chrome/hls.js, fresh start at 5.08 seconds | Skipped to about 6.10 seconds, then completed | Started near 5.08 seconds and completed |
| Native Safari, full segments | Reported ended near 5.07 seconds | Completed at 8 seconds |
| Native Safari, playlist with parts | Reported ended near 5.03 seconds; 126 frames | Completed at 8 seconds |
| Safari/hls.js, full segments | Paused before reaching the gap | Also paused early; inconclusive |

Chrome's largest presented-frame timestamp step was 80 ms in both continuous damaged cases.
The control's largest step was 40 ms.
These callbacks establish continued presentation, not pixel correctness or A/V synchronization.
The fixture is video-only.

The first Safari automation attempt timed out during a WebDriver click.
A separate manual-start attempt never played. Neither attempt counts as a media failure.
The reported native failures played to the gap and emitted `ended` before the expected endpoint.
This experiment does not identify the native Safari root cause.

## Decoder comparison

FFmpeg decoded 199 frames from the available damaged objects without warning messages.
The control decoded 200 frames.
Decoded-frame hashes differ for all 23 surviving pictures from 5.08 through 5.96 seconds.
Every decoded picture matches the control from the next IDR at six seconds onward.
Thus, successful playback does not mean the missing reference was harmless.
Concealment permits continuation, with altered pictures until the next restart point.

The decoder comparison concatenates available media objects. It does not exercise HLS GAP handling.

## Reproduction

Export the fixtures:

```sh
RUSHLS_GAP_FIXTURES=/tmp/rushls-video-gap \
  cargo test -p rushls --lib ip_video_gap_playback_fixture -- --ignored
```

Compare decoded frames and packet integrity:

```sh
python3 tools/check-video-gap-decode.py \
  --fixtures /tmp/rushls-video-gap --output /tmp/video-gap-decode.json
```

With ChromeDriver on port 4446, test an official hls.js bundle:

```sh
python3 tools/check-gap-playback.py \
  --fixtures /tmp/rushls-video-gap --hls-js /path/to/hls.js \
  --browser chrome --webdriver http://127.0.0.1:4446 \
  --video --start 0 --playlist master-full.m3u8 \
  --output /tmp/video-gap-chrome.json
```

Use `master.m3u8` for the playlist containing parts.
Use `--start 5.08` for fresh playback after the missing frame.
For native Safari, use `--browser safari --modes native --webdriver http://127.0.0.1:4445`.
The optional `--manual-start` flag waits for a visible Start playback click.

## Scope and next checks

This result justifies further experiments; it does not establish broad player support.
Remaining checks include live LL-HLS, audio synchronization, switching, repeated losses, and startup near gaps.
HEVC, AV1, and reordered H.264 remain untested by this fixture.
The permissive-mode decision must distinguish preserved packet delivery from guaranteed decoded picture quality.

## Native Safari isolation experiments

A follow-up matrix used the same packets to separate container boundaries from loss signaling.
All cases used native Safari, completed full-segment playlists, and the same local HTTP server.
No decoder, player, or production code was changed.

| Variant | Result | Observed continuation |
|---|---|---|
| Original 40 ms GAP and dependent restart | Failed, including two repeat runs | Ended near 5.04 seconds |
| Same playlist, discontinuity before the dependent restart | Passed, including two repeat runs | Last pre-gap picture at 5.00; next at 6.00 |
| Expand GAP to next IDR, without discontinuity | Failed | Ended near 5.97 seconds |
| Expand GAP to next IDR, with discontinuity | Passed | Resumed at six seconds |
| Merge available chunks into the original parent; retain timestamp hole | Passed, including two repeat runs | 80 ms presentation step |
| Mark the entire affected parent GAP; restart at IDR | Failed | Ended near 5.96 seconds |
| Restore missing picture, retain the same short segment cuts | Passed, including two repeat runs | Normal continuation |
| Remove separate GAP entry; include its time in preceding EXTINF | Passed twice | 80 ms presentation step |

The last variant changes playlist timing descriptions, not encoded sample durations.
It does not explicitly signal the absence and is only a diagnostic control.
The merged parent also lacks an internal HLS GAP marker for full-segment clients.
Neither control is a proposed replacement for explicit loss reporting.

Packet hashes, PTS, DTS, and durations match across the damaged variants that preserve all surviving packets.
The restored control matches every packet in the uninterrupted control.
Thus, neither re-encoding nor timestamp rebasing explains the different outcomes.

Request logs show that Safari fetched both the dependent segment and the later IDR segment in the failing exact-GAP case.
It did not request the unavailable GAP resource.
The observed failure therefore occurs after those resources are requested, rather than because the playlist hides them.
These tests do not establish Safari's internal failure mechanism.

### Video-only workaround (superseded as a general recommendation)

Adding `EXT-X-DISCONTINUITY` immediately before the resumed dependent segment allowed automatic completion.
Initialization bytes and all surviving media timestamps stayed unchanged.
Safari did not present the surviving dependent pictures from 5.08 through 5.96 seconds.
It resumed at the next IDR, at six seconds.
The two repeat runs had 998 ms and 1,016 ms between those frame callbacks.
The playback clock continued; this was approximately one second without a new presented picture, not an immediate seek.
This preserves delivery of those packets, but does not preserve their presentation in Safari.

Waiting for an IDR without a discontinuity did not fix the tested failure.
The result does not support treating IDR-only resumption as a sufficient workaround by itself.

A production discontinuity path requires coordination across renditions and correct discontinuity-sequence handling during retention.
[HLS discontinuity sequencing](https://datatracker.ietf.org/doc/html/draft-pantos-hls-rfc8216bis-22#section-4.4.3.3) supports synchronization across renditions.
This video-only test does not validate A/V continuity, rendition switching, or live LL-HLS behavior around such a marker.
It also does not make a dependent segment start conform to Apple's authoring requirements.

### Reproduce the variants

```sh
python3 tools/build-video-gap-variants.py \
  --fixtures /tmp/rushls-video-gap --output /tmp/rushls-safari-video-variants

python3 tools/check-gap-playback.py \
  --fixtures /tmp/rushls-safari-video-variants --hls-js /path/to/hls.js \
  --browser safari --modes native --video --start 0 \
  --playlist master-full.m3u8 --output /tmp/safari-video-matrix.json
```

Add `--filter '0[1257]-*' --repeat 2` to repeat the main comparisons.
The probe records resource requests alongside playback events and presented-frame timestamps.


## Independent controls and the audio distinction

The independent FFmpeg control disproves a general requirement to pair GAPs with discontinuities.
FFmpeg encoded twelve seconds of H.264 without B-frames and packaged six two-second HLS segments.
The damaged playlist marks the third existing segment unavailable and removes its resource.
All later segments, timestamps, and initialization remain unchanged. The next video segment starts with an IDR.

| Independent native Safari case | Result |
|---|---|
| FFmpeg fMP4, muxed audio/video, uninterrupted | Completed at 12.018 seconds |
| Same audio/video stream, third segment GAP | Completed at 12.040 seconds without discontinuity |
| FFmpeg video-only fMP4, third segment GAP | Ended prematurely near 5.93 seconds |
| Video-only MPEG-TS, uninterrupted | Completed at 12 seconds |
| Video-only MPEG-TS, third segment GAP | Ended prematurely near 5.96 seconds |

Some background repeat runs paused muted video, including uninterrupted controls.
Those runs are inconclusive and excluded from the media results above.
The automated matrix was stopped when background pauses prevented useful comparisons.
The VOD playlist-type variants therefore have no conclusive result.

### Exact Rushls video gap with separate audio

A second comparison adds a continuous AAC rendition to the existing eight-second Rushls fixtures.
Only the multivariant playlist and separate audio resources change.
The original exact-gap video playlist, initialization, segment bytes, and timestamps remain unchanged.
There is no discontinuity, timestamp rebasing, video repair, or extra video discard.

Native Safari completed the exact 40 ms video GAP twice, at approximately 8.007 and 8.093 seconds.
The matching separate-audio control completed at 8.021 seconds.
Both runs presented video after the gap, starting at the next IDR at six seconds.
They did not demonstrate presentation of the surviving dependent pictures from 5.08 to 5.96 seconds.
Frame callbacks were sparse, so their count is not a decoder output count.
These results demonstrate automatic continuation, not measured perceptual A/V synchronization.

The evidence points to a video-only playback-path difference, rather than a general GAP-plus-discontinuity requirement.
A role for audio in playback-clock handling is a hypothesis; the tests do not establish Safari's internal cause.
Do not adopt discontinuities as the generic recovery policy based on the earlier video-only fixture.
Keep the video-only behavior as a separate compatibility issue.
Live LL-HLS, audio gaps concurrent with video gaps, and rendition switching remain untested here.

### Reproduce independent controls

```sh
python3 tools/build-independent-gap-controls.py \
  --output /tmp/independent-gap-controls \
  --rushls-variants /tmp/rushls-safari-video-variants

python3 tools/check-gap-playback.py \
  --fixtures /tmp/independent-gap-controls --hls-js /path/to/hls.js \
  --browser safari --video --modes native --start 0 --deadline 20 \
  --filter 'fmp4-av-*' --output /tmp/independent-gap-results.json
```

For Rushls with separate audio, use `--filter 'rushls-av-*' --playlist master-full.m3u8`.
Keep Safari visible. If automatic playback pauses, rerun with `--manual-start --deadline 60` and click Start playback.
Do not classify a paused control as a media failure.

## Live A/V follow-up

This section records the normalized-interval injection experiment. The detection integration below supersedes its implementation scope.

The corrected live fixture also supports this route for non-reordered H.264 with AAC audio.
These results do not enable production video GAP detection or change the audio release gate.

The fixture uses the real session, coordinator, muxer, store, and HTTP origin.
It produces 24 seconds, two H.264 variants, and two AAC audio renditions.
Video samples have a 40 ms duration, with an IDR every two seconds.
A test-only normalizer adapter replaces pictures at 5.04, 7.84, and 13.04 seconds with exact GAP intervals.
It retains every other sample and its original timing.
Audio packet removal passes through the production audio-gap detector.
The first audio rendition has holes at 5.333–5.461 and 7.680–8.107 seconds.
Thus, the second video hole overlaps missing audio.
The second audio rendition has separately offset holes.

Both video playlists contain three 40 ms GAP parents and no discontinuities.
The test-only projector omits the video independence tag.
The first audio rendition splits its second hole at the coordinated boundary, producing three GAP parents for two holes.

| Corrected live case | Result |
|---|---|
| Native Safari, overlapping A/V holes, two audio switches | Completed at 24.017 seconds; both switches observed |
| Native Safari, repeat without switching | Completed at 24.041 seconds |
| Chrome, official hls.js 1.7.3, overlapping A/V holes and audio/video switches | Completed at 24.021 seconds; requested switches observed |
| Chrome, same switching probe without intentional holes | Stalled at 23.175 seconds; did not complete within the probe deadline |

Safari resumed presented video at the next IDR after the 5.04 and 13.04 second holes.
In the no-switch repeat, those callback intervals were approximately 1.43 and 1.00 seconds.
The short hole near the eight-second IDR also continued automatically.
This supports continuation, not seamless playback or intact reference pictures.

The hls.js damaged run loaded live playlists and hundreds of part resources.
It showed transient presentation jumps during switching, including a roughly three-second interval late in playback.
The clean control also showed large jumps and a final stall.
Therefore, these tests cannot attribute every switching disruption to GAP handling.
The clean control's final parts were marked loaded; this is not proof of the previously isolated final-part loading bug.
Native Safari resource timing does not expose its media requests here, so the report does not claim which resources it chose.
No test measured perceptual audio/video synchronization or decoded pixel correctness.

### Fixture correction and scope

The paced source adapter originally removed a packet before awaiting its delivery time.
Session supervision could cancel that wait and lose the packet.
This caused unintended 80 ms video samples and sometimes invalid part partitions.
The corrected adapter retains the packet until the wait completes.
A paused-clock regression proves that cancellation preserves the queued packet.
The corrected damaged runs each recorded 1,200 video samples with 40 ms durations before intentional removal.

Earlier live runs using that adapter are exploratory evidence only.
Their loss patterns were not fully controlled. This also limits earlier live audio-only loss comparisons.
The static fixture results are unaffected.

This experiment still bypasses production video-gap detection, budgeting, and notices.
Those paths must be implemented and tested before release.
Reordered video, HEVC, AV1, video-only Safari, and broader seek/start behavior remain outside this live validation.
No codec decoding logic, timestamp rebasing, or blanket discontinuity was added.

### Reproduce the live case

Use a fresh ready-file path for each run. Start the origin:

```sh
RUSHLS_GAP_LIVE_READY=/tmp/video-live.ready RUSHLS_GAP_LIVE_VIDEO=combined \
  cargo test -p rushls --lib live_av_gap_browser_origin -- --ignored --nocapture
```

In another terminal, start the probe:

```sh
python3 tools/check-live-gap-playback.py \
  --ready /tmp/video-live.ready --hls-js /path/to/official-hls.js \
  --output /tmp/video-live.json --browser safari --mode native \
  --webdriver http://127.0.0.1:4445 --manual-start --switches audio
```

Click Start playback in the test window and keep Safari visible.
For Chrome, use `--browser chrome --mode hls.js --webdriver http://127.0.0.1:4446 --switches all` without `--manual-start`.
Set `RUSHLS_GAP_LIVE_CONTROL=1` on the origin command for the clean control.
Use `single` or `repeated` instead of `combined` to omit audio loss.
The origin saves its encoded input, structured session events, and session outcome beside the ready file.

The rushls suite passed: 978 unit tests and 35 integration tests, with 13 opt-in tests ignored.
The cancellation regression and focused CMAF tests passed.
Strict clippy still reports seven existing diagnostics outside the new fixture code.

## Detection-to-publication integration

The gated normalizer now creates video GAPs from source timestamp evidence.
The first candidate requires declared fixed H.264 cadence without presentation reordering.
Production activation remains disabled. Existing production video behavior is unchanged.

The cadence validator retains its rational grid, tolerance, rolling budget, and episode reporting.
Before accepting a hole, the normalizer validates incoming metadata, decode-clock agreement, projected endpoints, and arithmetic.
It closes the preceding picture at the expected presentation position and emits the exact missing interval.
The resumed packet retains its timestamp and payload. No replacement picture is generated.
Non-integral holes are supported; nominal-rate hints alone do not trigger GAPs.
Strict mode continues to reject cadence violations.

Video GAP notices use `method=gap` and zero replacement ticks.
Budget rejection includes the typed reason while retaining `video_cadence_violation`.
A session regression covers accepted compensation followed by a bad packet in the same batch.
It verifies notices and ended-session totals during pre-roll and live processing, lease release, and rejection before batch publication.

The RTMP regression parses real FLV sequence and sample messages through the RTMP adapter.
It removes a coded picture before normalization and checks strict rejection and exact permissive timing.
It does not exercise an RTMP network connection.
The live MPEG-TS fixture now removes packets before normalization instead of replacing normalized samples.
Its x264 configuration explicitly declares fixed cadence; the fixture asserts that codec discovery retains that declaration.

Native Safari completed the integrated overlapping-loss case at 24.039 seconds, including both audio switches.
Both video variants published three 40 ms GAP parents without discontinuities.
The session verified six video compensation notices, two episode-entry notices, and exact repair-count metrics.
All replacement ticks were zero.

The tests also cover fractional timestamp rounding, missing DTS, non-integral lateness, budget boundaries, extreme arithmetic, recovery hysteresis, and reordered-PTS exclusion.
The full suite passed with 988 unit tests and 35 integration tests.
The AAC/Opus GAP decoder integration passed. Strict clippy retained seven existing diagnostics outside this change.

The `early` live scenario moves the first video hole to 0.40 seconds, during pre-roll.
The probe now gives manual activation a separate startup deadline, so a delayed click cannot consume the playback observation period.

Remaining activation work includes the codec/player matrix, seeks and fresh starts, video-only Safari, and capability-based independence signaling.
The live experiment still suppresses the independence tag through its test-only projector setting.
Do not enable the normalizer gate alone and leave that production playlist claim unchanged.
Reordered video, HEVC, and AV1 retain their existing paths.

Additional integrated player checks:

- Official hls.js completed the overlapping-loss case without switching at 24.021 seconds.
- Its switching run stopped at 23.851 seconds with the last video part still unloaded.
- The early-hole backend completed normally and verified its notices and metrics.
- The first Safari early-hole attempt started too late for the old observation deadline.
- The second aborted before presenting a picture. Safari reported hidden visibility and `AbortError`; this is not a successful playback validation.
- Official hls.js presented the early-hole stream through 23.960 seconds but did not emit `ended` before the deadline.

These results retain the production gate. They do not establish a new discontinuity requirement.

The earlier diagnostic hls.js final-part patch did not resolve the early-hole endpoint case.
That run stopped at 23.953 seconds without `ended`.
Therefore, the previously isolated final-part bug is not a sufficient explanation for every remaining completion failure.
The diagnostic bundle is not shipped, and these runs do not authorize production activation.

## Production video-only follow-up

The live fixture accepts `RUSHLS_GAP_LIVE_VIDEO_ONLY=1` to omit both audio renditions.
It retains the same two video encodings and uses production normalization and publication.
Use `RUSHLS_GAP_LIVE_VIDEO=single` for one missing picture per variant.
Add `RUSHLS_GAP_LIVE_CONTROL=1` for the uninterrupted control.

The native Safari control completed at 24.0046 seconds with 589 presented-frame callbacks.
Earlier control attempts were blocked by autoplay or did not advance.
The damaged run did not advance beyond its first picture and remained hidden.
It is inconclusive: this run does not reproduce the earlier mid-stream termination.
The probe now distinguishes failure to start from failure after playback advances.

The captured production video contains 599 surviving packets, one 40 ms GAP, and no discontinuities.
Packets retain 40 ms durations. Presentation timestamps skip from 5.00 to 5.08 seconds.
The next IDR remains at six seconds. Thirteen available parents remain.
The origin completed successfully and verified compensation notices and metrics.

Results: `/tmp/safari-prod-control3.json` and `/tmp/safari-prod-gap.json`.
Captured output: `/tmp/safari-production-gap-export`.
Further native playback testing requires a working foreground Safari session.
No production behavior was changed based on these inconclusive playback attempts.

### Foreground retry: failure reproduced on production output

The captured 24-second production video-only stream was played directly in Safari 26.6.2.
The test used a normal Safari window, without WebDriver playback setup.
The user also observed the final displayed picture at frame 125, timestamp 5.000 seconds.

| Case | Result | Last/next displayed picture |
| --- | --- | --- |
| Exact GAP, first run | Premature `ended` at 5.022500 seconds | Last picture at 5.00 seconds |
| Exact GAP, repeat | Premature `ended` at 5.020239 seconds | Last picture at 5.00 seconds |
| Discontinuity before resumed segment | Completed at 24.050810 seconds | 5.00 then 6.00 seconds |

The diagnostic control added only `EXT-X-DISCONTINUITY` after the GAP parent in both video playlists.
Initialization, media bytes, timestamps, segment durations, and GAP coverage stayed unchanged.
The control did not generate silence, stretch frames, rebase timestamps, or discard more input.
Safari did not display surviving dependent pictures between 5.08 and 5.96 seconds.

This reproduces the video-only compatibility problem on current production output.
It does not establish a general HLS requirement for discontinuities or identify Safari's internal cause.
The successful control covers completed playlists, not live playback or cross-rendition discontinuity coordination.
No production discontinuity behavior was added. Safari video-only GAP continuation remains a release limitation.

Saved reports:

- `/tmp/safari-production-gap-reproduced.json`
- `/tmp/safari-production-gap-repeat.json`
- `/tmp/safari-production-gap-discontinuity.json`

The diagnostic playlists and pages are in `/tmp/safari-production-gap-export`.
