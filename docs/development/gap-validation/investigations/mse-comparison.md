# Chrome completed-media replay and live comparison

Local evidence paths below are relative to the repository root and are not included in a clone.
See [validation artifacts](../artifacts.md) for storage and reproduction guidance.

Date: 2026-09-20. Follow-up to the [pending-part candidate](tracker-fix.md).

Completed-media replay succeeds, but the live uninterrupted control still stalls.
Chrome reports video demuxer underflow without a decoder error.
The evidence narrows the investigation to live delivery and appending. It does not establish the root cause.

The subsequent [append-order investigation](append-order.md) reproduces buffer holes from out-of-order and duplicate video appends.

## Completed-stream results

The capture contains unchanged Rushls initialization, segment, and retained part resources.
Each test selects the 160×90 H.264 rendition and the first mono AAC track.
Chrome 153.0.8010.50 runs headless on macOS 26.6.2.

| Playback path | Result | Actual part requests |
| --- | --- | --- |
| Direct MSE, full segments | Completed | 0 |
| Direct MSE, earlier segments and retained parts | Completed | 88 |
| Official hls.js 1.7.3, low-latency mode enabled | Completed | 0 |
| Official hls.js 1.7.3, low-latency mode disabled | Completed | 0 |

All four cases reach 24.021333 seconds and report 600 video frames, with zero dropped or corrupted frames.
Audio buffers cover 0–24.021333 seconds. Video buffers cover 0–24 seconds.
Chrome selects VideoToolboxVideoDecoder and FFmpegAudioDecoder.
The Media diagnostics contain no reported player errors or warning/error messages.

The completed playlists retain only recent parts. The direct part replay uses full segments for earlier parents without advertised parts.
It never appends both a parent and its parts.
An initial pilot omitted the earlier parents and could not start playback. That harness error is excluded from the results.
The corrected harness rejects a replay whose initial buffers omit the stream start.

Both hls.js cases load full segments despite their different low-latency settings.
They do not validate live part selection.
Direct MSE appends all data before playback, so it does not reproduce live append timing.

## Live control

The live control uses the existing local hls.js candidate, with no runtime selector override or requested rendition switches.
Its source transport stream has the same SHA-256 as the completed capture input.
This is a new origin run, not a replay of the earlier HTTP response schedule.

Playback stops at 23.178011 seconds without an ended event.
Both controllers finish, all final video parts report loaded, and MediaSource reaches `ended`.
Audio remains continuously buffered. Video has earlier holes, although its final buffered range covers 22–24 seconds.
Playback quality reports 258 total video frames, five dropped frames, and zero corrupted frames.

Chrome reports repeated video `DEMUXER_UNDERFLOW` events and a final pipeline underflow.
It reports no decoder error. This absence does not prove decoder state is correct.
The result reproduces the stall independently of intentional input GAPs.

The next diagnostic step is to trace actual live video append bytes, ordering, and buffer changes around the first hole.
Compare those bytes with the origin resources, then replay that append sequence directly through MSE.
This can distinguish omitted or overwritten frames from live decoder-state effects.
Do not change Rushls GAP policy based on this control failure.

## Reproduction and evidence

The case index (`target/rushls-validation/mse-comparison-2026-09-20/results.json`) records player hashes, buffer ranges, and playback quality.
The same directory contains compressed browser reports, Chrome Media diagnostics, origin events, and the captured fixture.
The capture and diagnostic scripts are retained with the evidence.

With Node.js 26 and ChromeDriver running on port 4446:

```sh
tar -xzf target/rushls-validation/mse-comparison-2026-09-20/fixture.tar.gz -C /tmp
node tools/compare-mse-playback.mjs /tmp/fixture /path/to/hls.min.js /tmp/mse-results.json
```

The tool targets this completed, uninterrupted H.264/AAC fixture. It is not a general codec or GAP conformance harness.
The fixture includes resource hashes. The reports record hashes for served resources.
The official completed-replay bundle and the patched live bundle differ, so this is not a controlled player-version comparison.

Validation: four completed browser replays, one live diagnostic reproduction, and JavaScript syntax validation.
Production Rust code is unchanged by this investigation.
