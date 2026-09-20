# Audio GAP validation

## Current implementation status

Production GAP handling is now active for audio and declared fixed, progressive H.264 without presentation reordering.
The test-only activation paths and blanket playlist independence claims have been removed.
Unsupported video mappings still reject cadence holes. Strict mode still rejects holes.
This activation does not resolve the player failures recorded below or establish CMAF conformance.
Earlier gate statements below describe the historical experiments.


## Release decision

The production gate remains closed. Decoder checks and completed-playlist checks do not establish safe live LL-HLS or A/V rendition behavior.
See [Audio gaps](audio-recovery.md) for the policy and reproduction commands.

## Automated checks

The `rushls` suite passes: 977 unit tests and 35 integration tests, with 12 tests ignored by their opt-in rules.
The two audio decoder checks were also run explicitly and passed.

Those checks cover AAC-LC mono/stereo at 44.1 and 48 kHz, Opus mono/stereo, HE-AAC mono/stereo, and HE-AAC v2 stereo.
HE-AAC fixtures use FFmpeg's AudioToolbox encoder at 44.1 and 48 kHz.
They check real media before and after missing packets. HE-AAC checks also decode resumed segments from a fresh decoder.

Changed Rust files pass formatting checks. The workspace formatting check finds existing differences in recording and shutdown integration tests.
Strict Clippy finds existing warnings in configuration, runtime, caption/catalog parsing, recording tests, and older CMAF tests.
These warnings were not suppressed in the source or included in this change.

## Browser scope

The browser probe uses Safari 26.6.2, its native HLS player, and hls.js 1.7.3 on Safari's media-source implementation.
It serves playlists exported through the Rushls muxer, store, and playlist projection.
The fixtures contain 200 ms parts and a hole made by removing six audio packets.

The probe checks playback from the beginning and startup at a position just after the gap.
It requires playback to reach the expected end. A buffered range, a `playing` event, or an `ended` event alone cannot pass the check.
It records nonfatal stalls and gap skips; a pass does not imply uninterrupted or silent playback.

An earlier probe gave hls.js a start position and then issued the same seek on `loadedmetadata`.
A focused 48 kHz stereo AAC-LC case passed after removing that duplicate seek.
The corrected matrix still exposes stalls for other configurations. The cause remains unresolved; do not attribute them to packaging or dismiss them as harness failures.

## Initial completed-playlist results

The corrected matrix ran 48 checks: 45 passed and three failed.
All 24 native Safari checks passed. hls.js passed 21 of 24 checks.
All failures occurred when starting just after a gap; playback from the beginning passed for every configuration.

| Failing hls.js fixture (codec clock and AudioSpecificConfig) | Requested start | Buffered range |
| --- | --- | --- |
| `Aac-44100-138856e5a0` | 0.753038549 s | 0.743038549–3.157913832 s |
| `Aac-44100-138856e5a54880` | 0.753038549 s | 0.743038549–3.157913832 s |
| `Aac-48000-130856e598` | 0.692666667 s | 0.682666667–3.157333333 s |

Each failed check remained at its requested position for the 12-second observation period and reported `bufferStalledError`.
The element was unpaused and reported ready state 4. Buffered media did not result in playback progress.
These are unresolved player-validation failures, not successful concealment.

A focused full-segment run repeated both 44.1 kHz configurations without LL-HLS tags: seven of eight checks passed.
The HE-AAC v2 configuration `Aac-44100-138856e5a54880` stalled again in hls.js at 0.753038549 seconds.
Thus at least one failure also occurs with full-segment playlists.

The fixture names retain the exact decoder configuration for reproduction:

```sh
python3 tools/check-gap-playback.py \
  --fixtures /tmp/rushls-gap-fixtures --hls-js /path/to/hls.min.js \
  --filter 'Aac-44100-1388*' --output /tmp/he-gap-playback.json
```

## Controlled follow-up

The initial failures do not establish a decoder-dependency problem.
The follow-up uses uninterrupted controls with the exact same encoded packets and initialization as the gap fixtures.
Only packet removal and the resulting segment boundaries differ.

The focused configuration is HE-AAC v2 at 44.1 kHz, with AudioSpecificConfig `138856e5a54880`.
Every comparison starts at 0.753038549 seconds, just after the gap endpoint at 0.743038549 seconds.
The full-segment multivariant playlist declares `mp4a.40.29`.
hls.js still reports `mp4a.40.2` for its source buffer, including in successful runs.
A codec label alone therefore does not explain the failures.

| Check | Result |
| --- | --- |
| Safari 26.6.2, hls.js 1.7.3, gap, fresh session per case | 3/3 passed |
| Safari 26.6.2, hls.js 1.7.3, uninterrupted control, fresh session per case | 3/3 passed |
| Chrome 153.0.8010.37, hls.js 1.7.3, gap, full segments | 3/3 passed |
| Chrome 153.0.8010.37, hls.js 1.7.3, uninterrupted control, full segments | 3/3 passed |
| Chrome, uninterrupted control with parts and low-latency mode | Repeated last-part loading; failed |
| Chrome, same control playlist with `lowLatencyMode: false` | Passed |

Safari reused-session probes sometimes paused or returned `NotAllowedError`, including for uninterrupted controls.
Fresh sessions avoid that interference in the focused checks.
The probe now records pause events, visibility, and playback-permission failures separately from playback failures.
It uses an audio element and fresh Safari sessions unless an existing session is explicitly supplied.
These changes do not prove that every earlier unpaused stall came from browser policy.
WebKit documents restrictions on [hidden silent-video playback](https://webkit.org/blog/7734/auto-play-policy-changes-for-macos/).

The Chrome low-latency failure does not require missing media or GAP tags.
hls.js repeatedly requests part 10 of the first parent, without progress into the next parent.
The buffered endpoint is 2.043355 seconds; the next declared part starts near 2.043356009 seconds.
This suggests an interaction with boundary precision or loaded-part tracking, but the exact cause remains unresolved.
A local diagnostic change adding 10 microseconds to part selection did not fix it.
No player patch or timestamp adjustment was added to production.

**Decision:** Continue the GAP approach. These checks weaken the case for codec-specific reconstruction.
Resolve the low-latency part-loading failure and complete live A/V validation before opening the gate.
Full-segment success is not a reason to disable low-latency playback silently.

The other two initially failing configurations also passed matched gap/control starts in both browsers:
HE-AAC mono at 44.1 kHz (`138856e5a0`) and 48 kHz (`130856e598`).
The final full-segment comparison set passed all 20 checks across the three configurations.
This remains a small, audio-only test set, not release certification.

Both decoder integration tests passed again after adding control exports.
Rust formatting and Python syntax checks passed for the changed test tools.
Strict Clippy still reports the existing warnings listed above; no new warnings appeared in this check.

### Reproduction

Regenerate the fixtures with the HE-AAC decoder test and `RUSHLS_GAP_FIXTURES` set.
The exporter now writes matching `-control` directories and multivariant playlists with codec declarations.

```sh
python3 tools/check-gap-playback.py \
  --fixtures /tmp/rushls-gap-investigation --hls-js /path/to/hls.min.js \
  --filter 'Aac-44100-138856e5a54880*' --modes hls.js \
  --start 0.753038549 --repeat 3 --playlist master-full.m3u8 \
  --output /tmp/safari-gap-controls.json
```

For Chrome, start a matching ChromeDriver on port 4446.
Add `--browser chrome --webdriver http://127.0.0.1:4446` to the probe command.
To reproduce repeated part loading, select the exact `-control` directory and use `--playlist index.m3u8`.
Compare that run with `--hls-config '{"lowLatencyMode":false}'`.
Keep the original hls.js bundle in both runs.

## Live origin follow-up and sequence fix

The repeated-part loop has a confirmed cause in hls.js 1.7.3.
Its [fragment eviction check](https://github.com/video-dev/hls.js/blob/v1.7.3/src/controller/fragment-tracker.ts#L156) treats sequence zero as absent.
A diagnostic correction passed twice. The unchanged player also passed twice when only the playlist's initial sequence changed to one.

Rushls now starts regular rendition media sequences at **1**.
This applies when each rendition is created; reconnects continue its existing sequence.
Media timestamps, segment URIs, part IDs, and I-frame sequence numbering do not change.
[HLS permits a declared initial sequence](https://www.rfc-editor.org/rfc/rfc8216#section-4.3.3.2).
No player patch, timestamp adjustment, new configuration, or discontinuity is required for this fix.
Delivery tests cover blocking reloads, reports, delta playlists, retention, and reconnects with the new initial sequence.

### Real live test

The ignored `live_av_gap_browser_origin` test serves the production HTTP origin and runs a real publishing session.
Its MPEG-TS fixture has two H.264 variants and two AAC-LC audio renditions, with 200 ms parts.
It removes six and twenty packets from each audio track after pre-roll.
The second audio track has different hole positions. One hole crosses a coordinated video boundary.
The test paces input, so the browser observes open parents, part publication, and blocking reloads.
It does not enable gaps in production builds.

Native Safari completed the 24-second stream and changed audio renditions in both directions.
The measured final video-frame/media-clock difference was about 38 ms, with transient samples up to 193 ms.
These callback measurements do not establish perceptual audio synchronization or bit-identical decoding.
Native Safari does not expose a standard API for forcing video variant selection; that part of the probe uses hls.js.

The default hls.js live checks remain blocked:

- Chrome can stop around 23.84 seconds with the final video part still unloaded after `ENDLIST`.
- This occurred with gaps even without audio or video switching. The uninterrupted Chrome control completed.
- Safari/hls.js can rewind during an audio switch. The uninterrupted control also rewound, so missing media is not required.
- The probe now rejects unsolicited rewinds, even if the player eventually reaches the end.

The final-part failure is distinct from the sequence-zero loop.
The player's [fragment selection](https://github.com/video-dev/hls.js/blob/v1.7.3/src/controller/base-stream-controller.ts#L1944) advances past a previously used parent after a live playlist ends.
A local diagnostic change keeps that parent eligible while it has unloaded non-gap parts.
With that change, the same live gap fixture completed in Chrome both without switching and with audio and video switches.
The switching run reached 24.021 seconds, confirmed the requested rendition changes, and had no unsolicited rewind.
This diagnostic bundle is not part of Rushls or a validated player distribution.
The production gate remains closed; default player failures still count as release blockers.

### Live reproduction

Start each case with a new ready-file name:

```sh
RUSHLS_GAP_LIVE_READY=/tmp/gap-case.ready \
  cargo test -p rushls --lib live_av_gap_browser_origin -- --ignored
```

In another terminal, start the probe with an official hls.js 1.7.3 bundle and ChromeDriver on port 4446:

```sh
python3 tools/check-live-gap-playback.py \
  --ready /tmp/gap-case.ready --hls-js /path/to/hls.min.js \
  --output /tmp/gap-case.json
```

Use `--switches none` or `--switches audio` to isolate switching effects.
Set `RUSHLS_GAP_LIVE_CONTROL=1` on the Rust process for the uninterrupted control.
For native Safari, add `--browser safari --mode native --webdriver http://127.0.0.1:4445 --manual-start`.
Click the visible Start playback button once. Use `--mode hls.js` for Safari's media-source path.
The JSON report includes controller state, loaded parts, playback samples, switches, requests, and final playlists.

## Remaining release checks

- Resolve the default hls.js final-part and audio-switching failures; repeat the live matrix.
- A/V synchronization across local audio segment cuts, including repeated holes and coordinated boundaries.
- Alternate-audio and variant switching with different sequence positions.
- One-frame and non-integral-frame holes in players, plus holes spanning several parts.
- Interactive seeking after playback has started, including entry near resumed decoder-dependent audio.
- Broader supported browsers and player versions.

Completed fixtures cover the wider codec set. The new live fixture covers H.264 with AAC-LC only.
Live HE-AAC, HE-AAC v2, and Opus checks remain outstanding.
Do not open `AUDIO_GAP_PLAYBACK_VALIDATED` until the remaining checks pass.
Do not substitute synthesis if a player fails to resume.

## Live fixture correction

The later video-gap investigation found cancellation loss in the test-only paced source adapter.
It removed a packet before awaiting delivery; a supervision wake-up could cancel that wait.
The corrected adapter retains the packet, with a regression test for cancellation.
Earlier live comparisons in this document therefore had uncontrolled additional packet loss.
Static decoder and completed-playlist tests are unaffected.
See [the corrected live A/V results](video-gap-validation.md#live-av-follow-up).
