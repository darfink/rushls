- Input tracks may in theory have PTS belonging to different epochs
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
- Run an LLM to extract DASH vs HLS to common share
- Run an LLM to revise types that should be domain and shouldn't
- Implement tests w/ mediastreamvalidator
- Extend the end-to-end test matrix to video, ELST/PDT offsets, reconnect fetch
  grace, and blocking reloads; the AAC AVFormat → HTTP path and ffprobe/Apple
  validation are covered.
- Support discontinuities when muxing? PaceToRealtime "maximum_timestamp_jump" doesn't make sense?
- Support closed-captions/SEI
- Delta playlists
- Do we want enriched content-types? E.g. codecs, charset etc
- Maybe re-use/ffmpeg refcounter buffers with cmaf muxer?
- Add h264 profile/level to accepted codecs logic?
- Test with non-paced source stream?
- Invert so http exports types for HLS instead of vice versa?
- Is a lot of work happening on one single thread?
- Let shutdown signal disconnect but flush? IDK

# MUST DOES

- Test reconnects/evictions?
- Automated test on macos w/ mediastreamvalidator?
- Support dynamic publishers
- Invoke callback once playlist exists?

# LATER

- IP bans??

# Problems w/ hooks:

An endpoint rejecting everything produces one node event per delivery. No worse
than the `eprintln!` it replaced, but if it floods, report transitions instead:
`HookUnhealthy` on the first failure after a healthy stretch, `HookRecovered`
when one lands again, plus a periodic aggregate. The per-event counters already
carry the rate, so the events only need to carry what changed.

Sign hook bodies with HMAC over the timestamp, event id, and the exact bytes
sent, so a consumer can tell a genuine delivery from anything that can reach
its URL. Needs a `headers` parameter on `outbound::HttpClient::post` and a
`signing_secret` (with the usual `_file` form) per hook. Sign after
serialization and keep those bytes, which the immutable envelope already does
for retries. mTLS is the alternative for deployments that prefer it.


protocol and remote_address aren't in session.started. Both are on PublishRequest but discarded before PublishGrant, so surfacing them is the session-model change your notes flagged, not a projection change