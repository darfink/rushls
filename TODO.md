- Support AV1 SVC
- Original IP-address for MoQ sources?
- Should we have IP rate limiting in app? Otherwise, w/o pacing, publisher can do 256 publishes to saturate capacity
- Warn instead of prevent non-HLS compliant settings?
- Input tracks may in theory have PTS belonging to different epochs
- Run an LLM against HLS spec and comment all spec related behavior
- Run an LLM to evaluate performance improvements + profile
- Run an LLM to find reductions in allocations
- Run an LLM to determine clean error types
- Add sustained-load, soak, reconnect-storm, malicious-input, and failure-injection tests
- Extend the end-to-end test matrix to video, ELST/PDT offsets, reconnect fetch
  grace, and blocking reloads; the AAC AVFormat → HTTP path and ffprobe/Apple
  validation are covered.
- Support discontinuities when muxing
- Add policy & allow connect-params to specify if CLOSED-CAPTIONS are present?
- Do we want enriched content-types? E.g. codecs, charset etc
- Add h264 profile/level to accepted codecs logic?
- Sometimes subtitle cue is layed over the other one - they collide?
- Ability to do normal HLS?

# Problems w/ hooks:

An endpoint rejecting everything produces one node event per delivery. No worse
than the `eprintln!` it replaced, but if it floods, report transitions instead:
`HookUnhealthy` on the first failure after a healthy stretch, `HookRecovered`
when one lands again, plus a periodic aggregate. The per-event counters already
carry the rate, so the events only need to carry what changed.

# Timestamp jumps & gaps

Recover bounded forward audio gaps through explicit gap/discontinuity handling
instead of permanently failing the session. Preserve elapsed time and A/V offsets;
never close the gap by shifting later audio backward or by stretching packet
durations. Keep snapping timestamp quantization noise to the sample clock. Reject
backward jumps and unexplained epoch changes until there is an explicit recovery
policy. This needs coordination across normalization, muxing, segment boundaries,
and delivery, not merely relaxing ensure_near (see docs/config-implementation.md).
Related but distinct: different initial track epochs, where independently zeroing
tracks could erase a legitimate A/V offset. The current asymmetry is deliberate:
extending a video frame display time is meaningful, while extending an encoded
audio packet duration does not manufacture missing samples. Frame size is
configurable, and Opus can use variable packet durations with a larger timestamp
tolerance.
