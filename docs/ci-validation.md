# CI validation

The **CI and release** workflow uses Node 24 actions. Dependabot checks for action updates each week.
Rust toolchain installation uses a composite action, with no JavaScript runtime.

## Checks and prerequisites

| Job | Checks | Prerequisites |
| --- | --- | --- |
| Validate code | Workspace tests, all features, Clippy, formatting, Python unit tests | Rust and Python |
| Validate media | Ignored decoder tests, recording, RTMP ingest, video GAP fixture export | FFmpeg, including AudioToolbox AAC |
| Validate Chrome playback (patched hls.js) | Patched-player unit tests; live control and GAP streams, rendition switches, completion; official-release compatibility | Chrome, matching ChromeDriver, Node.js 24, pinned hls.js source and patch |
| Validate Apple HLS | Packaging cases, playlist diagnostics, two-hour live authoring audit | macOS, Apple HLS tools, FFmpeg, trusted localhost TLS |
| Validate GStreamer playback | Live control stream; decoded audio/video and end-of-stream | FFmpeg, GStreamer 1.28+ hlsdemux2, PyGObject |

Code, media, Chrome, and GStreamer checks run on pull requests and main-branch pushes.
Apple validation runs through a reusable workflow on main-branch pushes, version tags, and manual main-branch runs.
Pull requests do not receive the Apple installer and certificate secrets.
**Publish container image** requires all five validation jobs and all five native archive smoke tests to succeed for the same commit.
A failed or skipped Apple validation blocks publication.
The reusable Apple workflow also supports manual validation without image publication.
Each ignored Rust test has a job or driver that supplies its external dependencies.
Interactive Safari investigations, load benchmarks, and manual diagnostic tools remain separate from the automated test suite.

Apple tests normally skip on machines without the tools.
The Apple workflow sets `RUSHLS_TEST_REQUIRE_APPLE_TOOLS=1`, which makes missing tools a failure.
A missing or failed `hlsreport` command also fails that job.

## Apple tools on hosted runners

Apple directs users to authenticated [Developer downloads](https://developer.apple.com/download/all/?q=HLS).
The tools support CLI installation after download. They are not part of the Xcode command-line tools.
Installer 1.26.143.14 requires macOS 26, despite the macOS 15 requirement in its bundled README.

Configure these repository variables for Apple validation:

- `APPLE_HLS_TOOLS_URL`: The download URL for an installer or its GPG-encrypted copy.
- `APPLE_HLS_TOOLS_SHA256`: The SHA-256 of the decrypted installer.
- `APPLE_HLS_RUNNER`: Optional macOS runner label; defaults to `macos-26`.

The compact audit requires 6 GiB of free temporary storage and checks this before encoding and after compilation.
GitHub [documents only 14 GB](https://docs.github.com/en/actions/reference/runners/github-hosted-runners) for its standard macOS runners.
The compact fixture targets standard hosted runners; the space check accounts for the build already on disk.
A storage prerequisite failure does not count as a completed audit.

`tools/install-apple-hls-tools.sh` downloads, verifies, and installs the package.
Rushls stores the encrypted installer as an asset in the `ci-tools` GitHub release.
The `APPLE_HLS_TOOLS_PASSPHRASE` Actions secret contains its decryption key.
The workflow decrypts the installer in a private temporary directory and checks the original SHA-256 before installation.
The installer and key are excluded from caches and report artifacts. Temporary installer files are removed when the script exits.
If no installer URL is configured, the script accepts tools already installed on `PATH`.
If tools are unavailable, both automatic and manual runs fail with setup instructions.
No missing-tool run counts as conformance evidence.

The workflow and local tests do not modify Keychain or require sudo for TLS.
For HTTPS, supply `RUSHLS_TEST_TLS_CERT`, `RUSHLS_TEST_TLS_KEY`, and `RUSHLS_TEST_TLS_HOST`.
The hostname must resolve to `127.0.0.1`; an additional `::1` record is allowed.
All addresses must be loopback addresses, and macOS must already trust the certificate chain.
A public DNS record and a publicly trusted DNS-01 certificate meet these requirements without hosts-file edits.
See [Let's Encrypt DNS-01](https://letsencrypt.org/docs/challenge-types/#dns-01-challenge).
For CI, configure the `APPLE_HLS_TLS` secret with JSON fields `certificate` and `private_key`, plus variable `APPLE_HLS_TLS_HOST`.
The pair changes atomically. See [certificate renewal](certificate-renewal.md) for the scheduled renewal job.
The credentialed Apple audit runs only on the default branch; fork pull requests do not run it.
CI uses TLS 1.3 and uploads reports even when conformance checks fail.
Version 1.26.143 reports missing TLS delivery even for Apple's public HTTPS example.
It also reports missing I-frame `HOLD-BACK` when sibling playlists contain partial segments, despite explicit hold-back values.
Version-specific exceptions require independent checks. The workflow preserves every original finding and severity.
Without TLS credentials, tests use HTTP and the strict audit reports transport failures.
Apple's validator has no documented custom-CA argument; curl's `--cacert` only affects harness requests.
An existing trusted CA can still be supplied through `RUSHLS_TEST_CA_CERT` and `RUSHLS_TEST_CA_KEY`.

## Two-hour live audit

Run:

```sh
tools/check-apple-authoring.sh
```

The script synthesizes six H.264 variants, from 234p to 540p, and two stereo AAC renditions.
The audio renditions contain distinct test tones, labelled English and Spanish; they do not contain translated speech.
A real TCP publisher sends Enhanced RTMP OneTrack messages with separate track IDs and language metadata.
Timed AMF `onCaption` messages create the English subtitle track through the production RTMP adapter.
No test wrapper injects tracks or subtitle packets after discovery.

The ladder includes a 48 kb/s video fallback with 96 kb/s audio for cellular delivery.
It retains video and subtitles because AirPlay 2 forbids audio-only variants.
The five other video variants use 145, 365, 730, 1,100, and 2,000 kb/s.
The 2,000 kb/s variant remains first. Higher-bitrate variants are omitted to fit free hosted runners.

The encoded period is 32 seconds: 960 video frames and 1,500 AAC frames at 48 kHz.
The publisher removes AAC encoder priming and derives timestamps from frame counts.
Repeated periods therefore do not accumulate container rounding or encoder padding.
Generated templates remain under `target/apple-authoring/synthetic-rtmp-v2` for repeat audits.

It publishes 7,212 seconds of media at burst speed, then continues at real-time speed.
Before validation, every advertised media playlist must contain at least 7,200 seconds and have no `EXT-X-ENDLIST`.
The test also verifies all six variants, both audio renditions, the subtitle language, and retained WebVTT cue text.
The configured retention is 121 minutes, with 512 MiB of memory and a 5 GiB disk overflow budget.
Allow at least 6 GiB of free temporary storage after compilation.
CI disables incremental compilation and debug symbols to limit build storage.
The disk tier removes its generated media when the origin shuts down.
This is a media-duration test, not a two-hour wall-clock endurance test.
Burst input also advances program dates from the publication's current wall-clock anchor; it does not simulate historical capture dates.

Both original reports remain in `target/apple-authoring/` locally, or the configured `RUSHLS_TEST_REPORT_DIR`.
CI uploads original and compatibility JSON/HTML plus request traces as `apple-hls-reports`, including reports from failed runs.
The artifacts also include the playlists, measured DVR windows, and a retained subtitle segment.

## Pass criteria and known tool defects

A passing job means that the tested contract passed with the documented exceptions.
It does not mean that Apple reported no findings or that every Apple requirement passed.
Both suites use strict reporting in CI. Unknown findings and genuine defects fail the job.

The two-hour audit permits two deployment recommendations: stream failover (`1040`) and the cellular default variant (`1083`).
The fixture tests one origin and keeps its 2,000 kb/s default variant.
All other recommendations remain subject to the audit's checks.

Tool exceptions apply only to validator `1.26.143 (1.26.143-260527)` and JSON schema `1.3`:

| Finding | Evidence required for an exception |
| --- | --- |
| TLS delivery (`1041`–`1043`) | Every reported URL uses one HTTPS origin. An independent curl request passes certificate verification and returns HTTP 200. |
| Missing I-frame HOLD-BACK (`-50096`) | Every affected playlist is I-frame-only. Both the JSON value and a fresh playlist meet `HOLD-BACK >= 3 * TARGETDURATION`. |
| Missing rendition reports (`-50125`) | Only one regular media playlist exists. There is no other required rendition to report. |
| HE-AAC identified as AAC-LC (`1051`) | The exact codec mismatch applies to all audio renditions. Independent ffprobe checks identify HE-AAC on every audio rendition. |

The TLS investigation found null security values that the validator exports as false.
The validator created its TLS checker but never called it during the traced public Apple HTTPS scan.
The HE-AAC mismatch also occurred when FFmpeg packaged the same fixture without Rushls.
The [HLS draft, Appendix B.1](https://datatracker.ietf.org/doc/html/draft-pantos-hls-rfc8216bis-22#appendix-B.1) excludes the current playlist and I-frame playlists from required rendition reports.

A new tool version receives none of these exceptions until its behavior receives review.
A failed independent check fails CI. The original JSON and HTML remain unchanged.
Each case also saves `.judgement.json`, with its CI decisions and reasons.
The GitHub summary shows the two-hour exceptions separately from the original findings.

The legacy subtitle MIME exception remains limited to subtitle-only `text/vtt` versus `text/plain` findings.
Version 1.26.143 no longer emits that finding.
Requirements that Apple does not validate remain visible and do not count as verified.

Packaging tests have explicit exceptions for their input: missing captions, unsupported codecs, unusual GOP cadence, and deliberate dependent-frame recovery.
Ordinary cases use strict input handling and enforce segment independence.
Only GAP recovery cases use permissive handling. The two-hour audit does not inherit their exceptions.

## Independent segments

`[accept] strict = true` is the default, including for named policies.
When all configured policies are strict, it also enables an enforced independence contract.
The origin advertises `EXT-X-INDEPENDENT-SEGMENTS` in master and media playlists.
The store refuses dependent first parts and direct segments before they become readable.
Later parts may depend on earlier parts within the same segment.
The contract survives gaps and publisher takeovers.

Set `strict = false` to allow the existing dependent-frame GAP recovery.
Strict sessions reject dependent starts even when another policy is permissive.
If any configured policy is permissive, manifests omit the global independence promise.
A later takeover can select that policy, while earlier segments remain retained.
The dedicated authoring audit uses strict input handling and enforces independence.

## Initial HTTP audit evidence

The initial HTTP Enhanced RTMP run retained 7,206–7,212 seconds across all thirteen rendition playlists.
The lowest video variant advertised a peak bandwidth of 161,818 bit/s, including audio and subtitles.
Apple's report cleared ladder coverage, the general default bitrate, segment independence, subtitles, and the cellular bandwidth limit.
The WebVTT sample contained the original `onCaption` text, including its separate initialization header.

That HTTP audit failed on these findings:

- HTTPS and HTTP/2 delivery: the local run used HTTP because no trusted localhost certificate was configured.
- Stream failover: this fixture publishes to one origin.
- The 730 kb/s cellular default: this playlist uses the general 2 Mb/s default.

Apple's [current specification](https://developer.apple.com/documentation/http-live-streaming/hls-authoring-specification-for-apple-devices/) gives different default recommendations for general/Wi-Fi and cellular delivery.
A single fixed playlist order cannot satisfy both defaults in the tool's combined report.
These findings remain failures; they are not added to the MIME exception.
The report does **not** establish full Apple authoring compliance.

All 1,102 ordinary workspace tests passed, with 15 external tests ignored in that command.
The dedicated two-hour audit ran separately and preserved its failing report.
Clippy, formatting, and workflow validation also passed.

The live Chrome probe also found a failure in the control case with hls.js 1.7.3.
All scheduled switches completed, but playback stalled before the end.
The matching GAP case completed successfully.
The original job preserved these results and failed. The current policy below separates the patched player from official-release compatibility.
The [earlier append-order investigation](gap-append-order.md) identified player-side ordering defects.
The browser fixture now also respects each demuxed track clock when pacing packets.
Regression tests cover cancellation without packet loss and both 48 kHz and 90 kHz timing.

### Corrected harness comparison

The corrected pacing ran with both the official hls.js 1.7.3 bundle and the existing local player candidate.
The comparison changed no production Rushls code or CI player dependency.

| Player | Control | GAP case | Scheduled switches |
| --- | --- | --- | --- |
| Official 1.7.3 | Stalled at 23.158 s, 259 frames | Completed at 24.021 s | All four observed in each case |
| Local candidate | Completed at 24.021 s, all 600 frames | Completed at 24.021 s | All four observed in each case |

The candidate's GAP run reported 574 presented frames. Completion does not establish perceptual concealment quality.
Local reports and player hashes are in `target/rushls-validation/ci-resume/`; those artifacts are not tracked by Git.
### Current Chrome CI policy

CI builds the [player correction](../tools/patches/hls.js/README.md) from pinned upstream source and a tracked patch.
The builder uses the upstream npm lockfile and identifies the result as `1.7.3-rushls.1`.
All upstream unit tests and both patched-player playback cases must pass before image publication.
The unchanged official 1.7.3 release runs separately as an informational compatibility check.
Its failure is visible in the job summary and is not treated as successful playback.

The `live-gap-playback` artifact contains both sets of reports, the patch, the unit-test log, and build provenance.
No Rust playback assertion is relaxed for the patched build.
Remove the local patch and make the official release blocking again once an upstream release includes the correction.

## HTTPS audit observation

The September 23, 2026 run used a Let's Encrypt certificate issued through LocalCert DNS-01.
Apple's validator accepted the chain without Keychain changes or sudo.
All 13 renditions retained at least 7,212 seconds.
The raw JSON records secure delivery, TLS version `0x0304` (1.3), and cipher `0x1302` (`TLS_AES_256_GCM_SHA384`).
The cleartext and HTTP/2 findings disappeared.

The installed report tool still flags TLS 1.2 and insecure cryptographic primitives.
These conflict with the recorded TLS negotiation; the leaf certificate uses an ECDSA/SHA-384 signature.
The original HTML and JSON remain unchanged, and these findings are not automatically waived.
Failover and cellular default-variant findings remain.
This initial HTTPS run also reported partial-segment HTTP 404 responses.
The investigation below resolves those audit failures.
It is evidence of working trusted HTTPS, not a passing authoring audit.

## Partial-segment fetch timing

A traced HTTPS audit reproduced 70 failed part requests across 31,374 delivery requests.
Every failed part had already disappeared from newer playlist responses.
Apple fetched some initial-playlist parts about 27 seconds after receiving that playlist.

The production policy keeps a removed part fetchable for three target durations: 18 seconds for this fixture.
This follows the minimum in [HLS section 6.2.2](https://datatracker.ietf.org/doc/html/draft-pantos-hls-rfc8216bis-22#section-6.2.2).
The exhaustive validator can outlast that grace while scanning many variants and I-frame entries.

The dedicated audit now retains removed part URLs for 60 seconds, twice its 30-second scan budget.
Part-tag visibility, production defaults, and error classification are unchanged.
A paused-clock regression verifies delayed fetches after tags disappear and expiry at the configured deadline.

The repeated HTTPS audit recorded 34,271 delivery requests, including 366 part requests, with zero delivery errors.
All thirteen rendition windows exceeded two hours, and the Apple report contained no 404 findings.
Other authoring findings remain failures.

`tools/check-apple-authoring.sh` saves `two_hour_live_authoring.requests.jsonl` beside the original reports.
Each entry records request and response times, the path, errors, and advertised part tags.
Set `RUSHLS_TEST_TRACE_DIR` to collect the same evidence from a shorter fixture.
The trace does not log query values or certificate material.

## What the failover finding checks

`hlsreport` consumes the JSON from `mediastreamvalidator`; it does not perform an outage test.
In local probes of version 1.20.7, the failover finding depended on the `failoverDataIDs` field in each variant.
Adding that field, even as an empty array, removed the finding from copies of the saved JSON.
Duplicating variant records with different URLs did not remove it.
These synthetic probes describe the report's check, not a working failover deployment.
The original validation artifacts were not changed.

A passing report cannot prove that playback survives an origin failure.
That requires real backup delivery paths and a playback test that interrupts the primary path.
The authoring fixture keeps its 2,000 kb/s default variant; it does not reorder variants to chase the cellular recommendation.

## TLS versions and the legacy report tool

HTTPS defaults to TLS 1.3. Under `[http.tls]`, set `version = { min = "1.2", max = "1.2" }` for TLS 1.2 only.
Set only the minimum to `"1.2"` to accept both versions, with TLS 1.3 preferred.

For the Apple audit, supply trusted certificate paths and set:

```sh
RUSHLS_TEST_TLS_MIN_VERSION=1.2 RUSHLS_TEST_TLS_MAX_VERSION=1.2 tools/check-apple-authoring.sh
```

The command above audits TLS 1.2. The current Apple CI job uses TLS 1.3 only.
The legacy report does not establish TLS 1.3 compliance.
Rustls retains its cipher defaults. No Keychain changes or privileged operations are needed.

The local TLS 1.2 test negotiated `TLS_ECDHE_ECDSA_WITH_AES_256_GCM_SHA384` (`0xC02C`).
That removed the insecure-cryptography finding, but the original report still flagged the TLS version.
Version 1.20.7 expects the old `SSLProtocol` enum value `8` for TLS 1.2.
Its validator instead records `tls_protocol_version_t` value `771` (`0x0303`).
Apple's `SecProtocolTypes.h` defines both values as TLS 1.2.
A controlled change of that field alone removed the warning.

The harness preserves the original `.json` and `.html` artifacts.
For this exact tool build and JSON schema 1.1, it also writes `.hlsreport-compat.json` and `.hlsreport-compat.html`.
The compatibility JSON converts only TLS 1.2's numeric representation; all other values and findings remain unchanged.
The HTML includes a note that explains the conversion.
The harness judges this compatibility report when available, while retaining the original validator findings.
Unknown tool versions, schemas, and TLS versions receive no conversion or exemption.
Cipher suites are never changed.

The full September 23 TLS 1.2 audit completed in 459 seconds with 32,442 requests and no delivery errors.
Its compatibility report has no TLS findings.
Failover, the cellular default variant, and the known WebVTT MIME recommendation remain visible.
The original report retains the enum-related TLS warning; the audit is not fully passing.

## Compact ladder storage check

The six-variant audit completed locally in 290 seconds on September 23, 2026.
All nine media playlists retained at least 7,206 seconds; 33,261 requests completed without delivery errors.
The report added no findings when the variants above 2,000 kb/s were removed.
In that earlier run, failover and the cellular default recommendation remained defects. The current pass criteria classify them as deployment recommendations.

Sampling filesystem allocation every ten seconds measured a peak DVR size of 3.60 GiB.
The local build occupied 3.22 GiB, and encoded templates occupied approximately 18 MiB.
This leaves room within a 13 GB budget; the actual hosted-runner check remains necessary after dependencies and compilation.
CI uses a separate compact-build cache and omits debug symbols and incremental output.
The audit keeps its two-hour assertion and 5 GiB disk cap; it does not waive retention to reduce storage.

The faster burst exposed an RTMP batch cancellation bug before validation.
RTMP reading now returns completed batches without yielding after packets have been consumed.
A regression test covers the cancellation boundary.

## GStreamer playback

`tools/run-gstreamer-ci.py` uses the same live Rust origin as the Chrome probe.
It plays the control case through `playbin3` and `hlsdemux2`.
The stream must reach end-of-stream without a pipeline error.
Both audio and video must produce decoded raw buffers spanning at least 12 seconds of the 24-second fixture.
The artifact includes buffer counts, timestamps, negotiated formats, warnings, and origin logs.
This check covers independent client decoding and completion; Chrome covers explicit rendition switching.

CI uses Ubuntu 26.04 for GStreamer 1.28.
Ubuntu 24.04 supplies 1.24.2, which decoded this control but failed to emit end-of-stream in CI.
On Linux, install the packages listed in `.github/workflows/ci.yaml`, then run:

```sh
/usr/bin/python3 tools/run-gstreamer-ci.py --output target/gstreamer-reports
```

GAP recovery is available separately with `--case gaps` or `--case all`; failures return a nonzero exit code.
It is not yet part of the required GStreamer check.
GStreamer 1.28.6 failed the local GAP case with consecutive fragment download errors near 5.3 seconds.
Its [playlist parser](https://github.com/GStreamer/gstreamer/blob/1.28.6/subprojects/gst-plugins-good/ext/adaptivedemux2/hls/m3u8.c#L1108) expects `EXT-X-GAP:` instead of the valid `EXT-X-GAP` tag.
This is a likely cause, not yet confirmed by a corrected GStreamer build.
The clean control decoded 600 video buffers and reached end-of-stream.
Chrome's required patched-player check continues to cover GAP playback.

Native binary packaging also runs on pull requests. See [tagged releases](releases.md) for the publication gates and archive checks.
