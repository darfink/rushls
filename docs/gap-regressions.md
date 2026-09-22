# GAP regression checks

The [GAP player matrix](gap-player-matrix.md) lists current evidence, support limits, and the remaining execution order.
Store generated captures under `target/rushls-validation/`; see [validation artifacts](validation-artifacts.md).

These checks distinguish packaging correctness from player continuation.
The browser results do not prove seamless concealment or perceptual A/V synchronization.

## Automated layers

The ordinary suite tests timestamp classification, budgets, intervals, lifecycle events, and GAP delivery.
It checks that dependent video parts remain unmarked after a gap.
Part independence returns at a verified random-access picture.

The Apple harness has four matched loss/control cases:

- Audio-only loss.
- Video loss with continuous audio.
- Overlapping audio/video loss.
- Video-only loss, whose Safari playback remains a known limitation.

All cases use the same eight-second H.264/AAC fixture.
The source wrapper drops one packet per selected track after 2.44 seconds.
Video loss removes a dependent picture. The fixture declares fixed cadence.
Controls retain every packet. Normalization, muxing, storage, and HTTP delivery are production paths.

Run the cases and export their output:

    RUSHLS_TEST_GAP_EXPORT_DIR=/tmp/rushls-regression-gaps cargo test -p rushls --test apple_hls gap_ -- --nocapture

Apple tools require macOS with mediastreamvalidator and hlsreport installed.
The harness skips when the required platform or validator is unavailable.
Do not interpret a skipped run as conformance evidence.

Reports retain known authoring exceptions under COMPATIBILITY EXCEPTION.
Missing independence flags on parts containing video sync pictures remain defects.
The exception label no longer uses “GAP,” which could be confused with missing media.

Exported fixture names ending in -control contain uninterrupted input.
The index.m3u8 playlist retains parts; index-full.m3u8 contains full segments only.
GAP resources remain absent. Export changes URI locations, not media bytes or timestamps.

## Browser checks

Use an official local hls.js bundle and a running browser driver.
For this fixture, the video hole spans 2.44–2.48 seconds.

Test startup before, inside, and after the hole, with a matched control:

    python3 tools/check-gap-playback.py --fixtures /tmp/rushls-regression-gaps --hls-js /path/to/hls.min.js --browser chrome --webdriver http://127.0.0.1:4446 --modes hls.js --video --playlist index.m3u8 --filter 'gap_video_with_audio*' --start 0 --start 2.42 --start 2.46 --start 2.50 --deadline 18 --output /tmp/gap-starts.json

Test seeking from continuous media into the missing interval:

    python3 tools/check-gap-playback.py --fixtures /tmp/rushls-regression-gaps --hls-js /path/to/hls.min.js --browser chrome --webdriver http://127.0.0.1:4446 --modes hls.js --video --playlist index-full.m3u8 --filter 'gap_video_with_audio*' --start 0 --seek-at 1 --seek-to 2.46 --deadline 18 --output /tmp/gap-seeks.json

Use --seek-to 2.50 to seek into resumed dependent video.
For Safari, select --browser safari --modes native --webdriver http://127.0.0.1:4445 --manual-start.
Keep Safari visible and click Start playback. Startup has a separate deadline.

Use the audio-only and overlapping A/V fixtures for the remaining matrix.
Omit --video for audio-only playback.
The existing live probe covers ongoing publication and rendition switching; completed fixtures cannot replace it.
The probe records every scheduled switch, including requests that never occur or cannot select a track.
A switch requires a distinct previous selection and a matching event after the request.
An event after the next request for that media kind cannot satisfy the earlier request.
If ABR already selected the planned video target, the probe requests the other variant and records the actual target.
For hls.js isolation, use `--fixed-level 0 --switches none --trace-endlist`.
The trace retains playlist response text, load events, and fragment-selection decisions.
It records a dropped-entry count if its 6,000-entry limit is exceeded.
A pinned run requires observed selection of the requested variant without another variant transition.
These diagnostics use hls.js internals. Check them again when changing the tested player version.
Native Safari runs use `--switches audio`. Unsupported native video switches cannot count as passing checks.
Run the probe regressions with:

    python3 -m unittest discover -s tools -p 'test*playback.py'


Track the Safari video-only failure separately:

    python3 tools/check-gap-playback.py --fixtures /tmp/rushls-regression-gaps --hls-js /path/to/hls.min.js --browser safari --webdriver http://127.0.0.1:4445 --modes native --video --manual-start --playlist index-full.m3u8 --filter gap_video_only --start 0 --deadline 18 --expect-premature-end --output /tmp/safari-video-only.json

This expectation requires playback to advance and then end prematurely.
Autoplay denial or failure to start cannot satisfy it.
An unexpected successful completion also fails the expectation, prompting review of the known-failure classification.
Run the corresponding control without --expect-premature-end.

## Recorded checks

The four Apple cases and their controls passed after correcting post-gap IDR part flags.
Official hls.js passed eight startup checks and two seek checks for video loss with audio.
Native Safari passed a full-segment A/V fresh start at 2.46 seconds.
Its first displayed frame was at 4.04 seconds, after the next IDR.
These checks do not complete the wider codec, live seek, and player matrix.
