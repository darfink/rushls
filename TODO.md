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
- Run an LLM to determine clean error types
- Run an LLM to revise SessionEvents
- Implement tests w/ mediastreamvalidator
- Extend the end-to-end test matrix to video, ELST/PDT offsets, reconnect fetch
  grace, and blocking reloads; the AAC AVFormat → HTTP path and ffprobe/Apple
  validation are covered.
- Support discontinuities when muxing? PaceToRealtime "maximum_timestamp_jump" doesn't make sense?
- Delta playlists
- Do we want enriched content-types? E.g. codecs, charset etc
- Maybe re-use/ffmpeg refcounter buffers with cmaf muxer?

# MUST DOES

- Config & CLI params (TLS and CORS are done: `RUSHLS_TLS_CERT`/`RUSHLS_TLS_KEY`
  with in-place certificate reload, and `RUSHLS_CORS_ORIGINS`/`RUSHLS_CORS_CREDENTIALS`)
- Test reconnects/evictions?
- Automated test on macos w/ mediastreamvalidator?
- Support dynamic publishers
- Verify not stalling on publish?
- Invoke callback once playlist exists?

# LATER

- IP bans??
