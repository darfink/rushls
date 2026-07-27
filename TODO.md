- Input tracks may in theory have PTS belonging to different epochs
- Run `StreamStore::maintain` on a timer once the server has a task to own it
- Playlist writer: EXT-X-DISCONTINUITY where `publication` changes, EXT-X-MAP from
  `RenditionSnapshot::initialization_for`. EXT-X-DISCONTINUITY-SEQUENCE also needs
  a count of discontinuities already evicted from the window — the store can track
  it, but pin the semantics against the spec when the writer exists.
- Normalize PTS to 0 - ELST for adjustments?
- Health evaluation doesn't consider track-local publication duration
- Run an LLM against HLS spec and comment all spec related behavior
- Run an LLM to evaluate performance improvements + profile
- Run an LLM to find reductions in allocations
- Run LLM to change all tests to -> Result + ? for condensing stuff
- Implement tests w/ mediastreamvalidator
- Implement end-to-end tests validating expected PTS, ELST, PDT, fetch, grace etc
- Implement end-to-end tests with ffprobe??
- Implement
- A single automatic cancellable maintenance task for the store

# Steps

- RTMP/SRT ingest
- Media normalization
- CMAF muxing
- HLS generation
- HTTP serving
- Process wiring (main.rs)

# Implement tests for:

Apple’s timing rules
The main LL-HLS values should be derived from the locked segmentation plan, not opportunistically from the first segment:
Rule	Value
Recommended part target	1 second
Part target versus network	At least P95 RTT; preferably at least 3× P95 RTT
PART-HOLD-BACK	Apple requires at least 3× part target
HOLD-BACK	At least 3× target duration
CAN-SKIP-UNTIL	At least 6× target duration
Part publication cadence	A new part within 1× part target
Segment publication cadence	A new segment within 1.5× target duration
Blocking reload deadline	After more than 3× target duration, return 503 if still unsatisfied
Excessively future MSN	More than last segment MSN + 2 → normally 400

# LATER

- IP bans??
