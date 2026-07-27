- Input tracks may in theory have PTS belonging to different epochs
- Run `StreamStore::maintain` on a timer once the server has a task to own it,
  and call `Origin::prune` on the same tick so playlist caches do not outlive
  the streams they describe
- Delta playlists: `CAN-SKIP-UNTIL` is deliberately never advertised, since
  advertising it commits the origin to rendering `EXT-X-SKIP`. Implement both
  together or neither.
- Confirm `EXT-X-MAP` on a WebVTT media playlist against mediastreamvalidator.
  It is spec-legal and is how the pass-through muxer separates the `WEBVTT`
  header from cue-only segments, but it is a less-travelled path in players.
- Normalize PTS to 0 - ELST for adjustments?
- Health evaluation doesn't consider track-local publication duration
- Run an LLM against HLS spec and comment all spec related behavior
- Run an LLM to evaluate performance improvements + profile
- Run an LLM to find reductions in allocations
- Run LLM to change all tests to -> Result + ? for condensing stuff
- Implement tests w/ mediastreamvalidator
- Implement end-to-end tests validating expected PTS, ELST, PDT, fetch, grace etc
- Implement end-to-end tests with ffprobe??
- Support discontinuities when muxing? PaceToRealtime "maximum_timestamp_jump" doesn't make sense?
- A single automatic cancellable maintenance task for the store
- Delta playlists

# Steps

- RTMP/SRT ingest
- Media normalization — done
- CMAF muxing — done
- HLS generation — done (`delivery::hls::project`)
- HTTP serving — done (`delivery::hls::serve`, `server::http`)
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
