# FLAC over Enhanced RTMP

Rushls accepts FLAC sequence headers and coded frames through RTMPX 3.1.
It publishes fragmented MP4 with `fLaC` sample entries, `dfLa` configuration, and `CODECS="fLaC"`.
This does not add FLAC ingestion over MPEG-TS or MoQ.

The initial implementation accepts mono and stereo, 4–32-bit samples, and rates from 1 to 65,535 Hz.
Admission policy can restrict those rates further. The reference configuration permits 44.1–48 kHz audio.
Higher rates need additional MP4 sample-entry handling and remain rejected.
Browser and native-player compatibility for this FLAC path remains unverified.
Decoder validation does not establish a CMAF media-profile conformance claim.

Each RTMP coded message can contain several complete FLAC frames.
Rushls parses frame boundaries and exact decoded sample counts, including short final frames.
It validates header and frame CRCs, channel assignments, sample rates, bit depths, and subframe structure before accepting the message.
Packed frames must have consecutive frame or sample numbers. Partial frames and configuration changes fail publication.
The existing packet-size and batch-count limits also bound message parsing and frame expansion.

Timestamps use the decoded sample clock with allowance for RTMP millisecond quantization.
Strict mode rejects missing audio. Permissive mode uses the existing bounded GAP policy and compensation events.
No samples are synthesized, and no surviving frame is extended.
The output STREAMINFO omits input-file totals and MD5 checksums, which cannot describe a live or gapped output.

## Validation

The checked-in fixtures cover mono 44.1 kHz/16-bit and stereo 48 kHz/24-bit audio.
Tests cover packed messages, live batches, rounding, strict rejection, permissive gaps, malformed frames, and MP4 durations.
The FFmpeg integration compares decoded PCM exactly for uninterrupted and gapped output.
It also starts a fresh decoder for each part, including parts after a gap.
Run it with:

```sh
cargo test -p rushls --lib flac_mp4_decodes_losslessly_and_resumes_after_gaps -- --ignored
```

The parser follows [RFC 9639](https://www.rfc-editor.org/rfc/rfc9639.html).
Packaging follows the [FLAC-in-MP4 mapping](https://github.com/xiph/flac/blob/master/doc/isoflac.txt).
