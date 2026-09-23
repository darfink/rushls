# Pending-part loading correction for hls.js

This directory contains the candidate upstream patch used by Rushls's required Chrome playback check.
The patch applies to hls.js v1.7.3, commit `e5ff3583965e3af16c4a4b4d2b5f7bd1ffb5b7de`.
The upstream code uses the Apache License 2.0; see [LICENSE](LICENSE).
The patch changes upstream source and unit tests. It has not been submitted upstream.

## Problem and proposed correction

During live playback, a parent fragment can appear buffered while some of its parts remain unloaded.
After an ENDLIST update or a rendition switch, the player can skip these parts and stop before the stream ends.

The correction checks pending non-GAP parts before advancing to another parent fragment.
It preserves loaded-part state across ENDLIST updates and uses actual buffer bounds when marking a parent complete.
Part selection no longer uses the full-fragment lookup tolerance, which can exceed a part's duration.
Unit tests cover parent boundaries, final parts, GAP exclusion, timestamp rounding, and ENDLIST updates.

## Reproduce and verify

Install Node.js 24, npm, Git, Python 3, Chrome, FFmpeg, and the Rust toolchain.
From the Rushls repository root, run:

```sh
python3 tools/build-hls-player.py --output target/player --unit-tests
python3 tools/install-chrome-driver.py --output target/chromedriver
python3 tools/run-live-gap-ci.py --driver target/chromedriver --hls-js target/player/hls.patched.js --output target/playback-patched
python3 tools/run-live-gap-ci.py --driver target/chromedriver --hls-js target/player/hls.official.min.js --output target/playback-official
```

Set `CHROME_BIN` if Karma cannot find Chrome automatically.
The builder fetches a pinned commit into a temporary directory, applies this patch, and installs the upstream lockfile with `npm ci`.
It builds the full player and sets its version to `1.7.3-rushls.1`.
The output includes source, lockfile, patch, and bundle hashes, plus Node/npm versions and the unit-test result.
The official release bundle has a fixed SHA-256 checksum.

Both playback runs use the same Rust origin and probe.
They publish 24 seconds of media, switch audio and video renditions, and transition from live playback to ENDLIST.
The control has no injected loss. The GAP case introduces deliberate media loss.
A passing run must finish playback, observe all four switches, and avoid fatal errors and time rewinds.

## Evidence for upstream review

In the macOS Chrome 153 investigation, the official v1.7.3 control stalled at 23.961 seconds.
The video buffer had a hole from 21.8 to 22 seconds and lacked the final video samples.
The patched control and GAP case both reached the end at 24.021 seconds.
All 1,211 upstream unit tests passed with the patch.
The official GAP result varies with timing; it passed locally but stalled in Linux CI.

Playback completion does not prove perfect GAP concealment or eliminate every redundant audio append.
Keep those questions separate from the missing-part correction.
See [the loading analysis](../../../docs/gap-live-order-fix.md) for the detailed investigation.

CI blocks on patched-player unit tests and both playback cases.
It also runs the official release as a separate informational compatibility check.
Its failure remains visible in the job summary and artifacts.
After an upstream release includes the correction, remove the local patch and restore the official release as the required player.
