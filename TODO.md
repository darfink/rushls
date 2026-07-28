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
- Extend the end-to-end test matrix to video, ELST/PDT offsets, reconnect fetch
  grace, and blocking reloads; the AAC AVFormat → HTTP path and ffprobe/Apple
  validation are covered.
- Support discontinuities when muxing? PaceToRealtime "maximum_timestamp_jump" doesn't make sense?
- A single automatic cancellable maintenance task for the store
- Delta playlists
- Do we want enriched content-types? E.g. codecs, charset etc

# MUST DOES

- SRT
- Test subtitles + multitracks
- Config, CLI params & TLS
- Test reconnects/evictions?

# LATER

- IP bans??
