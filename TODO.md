- Configurable part hold back - e.g. 500ms parts but 2s part hold back?
- Stream retention always at least as long as playlist window?
- JWT for signed streams
- Stuck shutdown
- Support DVR
- Warn instead of prevent non-HLS compliant settings?
- Input tracks may in theory have PTS belonging to different epochs
- What's the status of B-frames?
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
- Add sustained-load, soak, reconnect-storm, malicious-input, and failure-injection tests
- Implement tests w/ mediastreamvalidator
- Extend the end-to-end test matrix to video, ELST/PDT offsets, reconnect fetch
  grace, and blocking reloads; the AAC AVFormat → HTTP path and ffprobe/Apple
  validation are covered.
- Support discontinuities when muxing? PaceToRealtime "maximum_timestamp_jump" doesn't make sense?
- Add policy & allow connect-params to specify if CLOSED-CAPTIONS are present?
- Delta playlists
- Do we want enriched content-types? E.g. codecs, charset etc
- Maybe re-use/ffmpeg refcounter buffers with cmaf muxer?
- Add h264 profile/level to accepted codecs logic?
- Test with non-paced source stream?
- Is a lot of work happening on one single thread?
- Let shutdown signal disconnect but flush? IDK
- Store cached segments on disk?
- Support Shaka Player
- Sometimes subtitle cue is layed over the other one - they collide?
- Support MoQ ingest?
- Retention window should be in seconds?
- Pacing should enforce backpressure (e.g. reduce buffers earlier in pipeline)
- Default pacing maximum_lead should equal retention window?
- How does pacing work if first very slow, e.g., 0.5x., then burst 1.5x - is there leeway?
- Advantageous to dynamically grow `segment` with parts & serve byte-ranges?
- Ability to do normal HLS?
- Drop `strict`, just have `open` default unless auth hook added?

# MUST DOES

- Test evictions?
- Automated test on macos w/ mediastreamvalidator?

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


# Foo

One correction to add on the video side: I said the transcoder's CFR re-encode hid the video gap, which videorate confirms — but Rushls' video path is independently gap-tolerant anyway. It derives each frame's duration from the DTS step to the next packet (video.rs:165, stepped_by_dts), so a gap just becomes one long frame. Video is protected twice over; audio has no protection at all, because the audio normalizer accumulates fixed 1024-sample steps and compares
