> Current timestamp policy and failure hooks: [Timestamp failures](timestamp-failures.md).

# RFC: the Rushls configuration surface

Setting names use `max_*` for upper limits, `*_bytes` for byte counts, and
`*_file` for secret files. Service deadlines use `timeout`. TLS paths use
`cert`, `key`, and `client_cert`. Queue counts use `queue_size`.
Enable `[hls].scrubbing` to publish keyframe playlists for fast seeking.


> **Status: implemented**, apart from the features named as not built below:
> Payload-carrying hooks. Their keys are refused at startup
> rather than silently accepted.
>
> **The goal was inverted on purpose.** This document does not describe how to
> configure the internals. It describes the configuration an administrator
> would want if the internals did not exist yet — and the application was then
> changed to serve it. Where this document and the code disagree, the code is
> what moves.
>
> Replaces the earlier open discussion in `CONFIG.md`, now removed. That
> document contradicted this design in several places.

The surface is [rushls.reference.toml](../rushls.reference.toml).
That file is the contract. This document is why it is shaped that way, and it
records the promises the file makes but cannot explain in a one-line comment.

## What this is reacting to

The `rushls.toml` this replaced was a complete reference — every knob, every
default, an essay per field — pretending to be the file a new administrator
opens. Three consequences:

1. **The altitude is flat.** `listen` sits beside `publication_stall_multiplier`
   and hook `queue_size`.
2. **Limits are a scavenger hunt.** Hardening an untrusted edge means touching
   six tables, and several caps have no "unlimited" form.
3. **The file speaks pipeline.** `faster_than_realtime`, `maximum_lead`,
   `strict`, `source_stall` vs `media_stall`,
   `maximum_pending_publishers_per_listener`. These are internal type names
   transcribed into TOML.

The module doc already claims the opposite — *administrator intent, not the
shape of the internal pipeline*. `CodecValue` is the one place that honours it:
a narrower, config-only type with a `From` impl doing the translation. Every
section should look like that.

**The test for any field:** could the internal type be renamed or recarved
without touching the config? If not, the config is leaking.

## Design rules

1. **Compiled defaults are permissive.** No file at all yields a working local
   origin. Hardening is what you *add*, not what you dismantle.
2. **Two shipped files, not a `mode` field.** The file is the stance.
3. **Operator vocabulary only.** *publisher*, *stream*, *listener*. Never
   *publication*, *policy*, or a pipeline stage.
4. **One config field may fan out to several internal ones.** That is the
   translation layer earning its keep.
5. **`"off"` is legal for every operator limit.**
6. **Durations an administrator can read**, not multipliers to decode.
7. **Essays live here, not in the file.**

## The operator questions, in order

Where do I listen → who may publish → what media will I take → how does live
HLS behave → how hard do I cap this box → who else gets told.

The file follows that order. It is the order the questions occur in, not the
order the pipeline runs in.

## Listeners

RTMP and HTTP binds default to dual-stack `[::]`. SRT is IPv4-only
(`0.0.0.0`) because ingest uses `rsrt`, which has no IPv6 listener yet.
MOQ (`[moq] listen`) is **off** until an operator turns it on: WebTransport
needs a certificate, and the compiled default must boot without one. The usual
proxy posture is cleartext on loopback with TLS ended in front, rather than TLS
in this process. No trusted-proxy list is configured; header trust is not how
playlist URLs or auth are derived.

`public_url` only shapes playlist URLs:
empty means relative, which is right behind a proxy or CDN; a trailing slash
is insignificant. Certificates reload in place on rotation, with secure TLS
defaults and no cipher knobs. HTTPS and MOQ share that rotation machinery, but
not a `ServerConfig`: HTTP/3 requires TLS 1.3 and `h3` ALPN, so MOQ must not
reuse the viewer HTTPS config.

Direct exposure has two shared limits under `[http]`: `max_connections`
and `max_requests`, both defaulting to 4096. HTTP, HTTPS, and a separate
metrics listener share these budgets. Excess connections close immediately.
Excess requests receive `503`, `Retry-After: 1`, and `Cache-Control: no-store`.
A request keeps its slot while waiting for media and while sending its body.
HTTP/2 streams use the same request budget. These are global capacity limits;
per-IP policy belongs in auth or the deployment boundary.

The first request must reach the handler within 30 seconds of connection
admission, including protocol detection. Subsequent HTTP/1 headers also have
a 30-second read deadline. HTTP/1 read buffers and HTTP/2 header lists are
limited to 32 KiB. Each HTTP/2 connection permits up to 128 concurrent streams.
HTTP/2 keepalive checks run every 30 seconds, with a 10-second response deadline.
TLS retains its separate handshake budget and deadline.

Incompatible reconnects keep at most 64 retired renditions per stream.
Each final playlist stays for its retention window plus fetch grace, unless
stream capacity requires earlier removal. Reconnecting again does not extend
that lifetime. Empty retired renditions leave immediately; active renditions
keep their existing retention rules. The render cache holds at most 128 regular
and 128 I-frame playlist entries per stream. Their bytes share the
`memory_per_stream` budget with media. Eviction preserves responses that
already hold their media or rendered bytes.

MOQ identity matches SRT's last-`/` split: `https://origin/live/camera` is
namespace `live` and name `camera`; a single path component is a single-key
publish. The credential is the `token` query parameter when present, otherwise
the resource name. An empty CONNECT path falls back to the moq-lite SETUP path.
If both paths are empty, the first broadcast announcement supplies the resource.
The origin rejects a second broadcast on the same connection.

The listener accepts `moq-lite-05` over WebTransport (`https://`) and raw QUIC
(`moqt://`). WebTransport selects the version in the CONNECT response.
Raw QUIC selects the version through TLS ALPN. See [MOQ ingestion](moq-ingestion.md)
for supported media formats and a local publish test.

## `[accept]`

`[accept]` is admission: everything about whether a publisher is let in and on what
terms. The top level holds admission controls (`ceiling`, `stall`, `floor`,
`takeover`, covered under Publish speed below); the nested `[accept.video]`,
`[accept.audio]`, and `[accept.subtitles]` tables hold predicates. It is the
per-publisher deal, and the only table whose rules may act on a live session —
throttle it, drop it, replace it. Sizing the box itself is `[capacity]`, which
only ever refuses new work.

Every field under those nested tables is a **predicate over a candidate** —
the set of values admitted. There are exactly three constructors:

| You write | Means |
| --- | --- |
| `frame_rate = 60` | exactly 60 |
| `frame_rate = [24, 25, 30]` | one of those three |
| `frame_rate = { min = 24, max = 60 }` | inclusive range; either end optional |

`min` and `max` are inclusive. A float edge goes in the value
(`max = 59.94`), never in the operator.

**A scalar is exact, not a ceiling.** `tracks = 1` admits exactly one track.
"At most one" is `{ max = 1 }`. The parser never guesses, because a rule where
a bare value silently means a ceiling cannot be read without the schema in
hand.

This was argued the other way — bare-means-`<=` — and rejected on one case:
`channels = 2` reads as a specification (stereo), while `tracks = 8` reads as a
budget. A rule that is correct on four fields and misleading on the fifth is
worse than an explicit form everywhere.

**Arrays are set membership, never a range.** Enumerations are the common case
in this domain — codec lists, film versus PAL rates — and spending array syntax
on ranges would leave no honest way to spell them.

### Units and ordering

Omitting `codecs` and writing `{ preset = "common" }` are **not** the same
thing, and the difference is what makes the preset worth having. Omitting the
field admits every codec this origin can mux, including ones added by a later
release. The preset is **frozen** to the set as it stands today, so a stream
using a newly supported codec is refused until an operator opts in.

Omit when the admitted set should track what the software can do; name the
preset when it is part of a contract that should not widen underneath it.

Sample rate is **Hz as an integer**: `48000`, or `{ min = 44100, max = 48000 }`.
The friendly `"48kHz"` alias is accepted **anywhere a rate is written**,
including inside `{ min, max }`, and is converted before any comparison.
Accepting it in one position but not the other would be the worst of both.
String comparison is never used, because `"8kHz" > "48kHz"` lexicographically.

Frame rates are compared as **exact rationals**, never floating point. `29.97`
and `"30000/1001"` are the same rate and both are accepted, neither rounded
before comparison. The quotient form is a string; a bare `30000/1001` is not
valid TOML. `frame_rate = 30` therefore does not admit 29.97 — the predicate
doing what it says — and the range form is how a deployment accepts both.

That equivalence is an **alias, not arithmetic**. `29.97` read literally is
`2997/100`, which is not `30000/1001`; the two are the same rate only because
the broadcast decimals are conventional shorthand for the NTSC rationals, so
the parser maps them explicitly:

| Written | Parsed as |
| --- | --- |
| `23.976` | `24000/1001` |
| `29.97` | `30000/1001` |
| `59.94` | `60000/1001` |
| `119.88` | `120000/1001` |

Every other decimal is its literal value. The table is closed and short
because it covers exactly the spellings encoders report, and it lives in
parsing alone — comparison stays exact-rational and needs to know nothing
about it.

Comparing to a tolerance was the alternative and is rejected. A predicate that
admits within two decimal places makes `frame_rate = 30` quietly accept 29.97,
which is the predicate not doing what it says, and the mistake is invisible:
a deployment that meant exactly 30 receives NTSC sources indefinitely without
a signal. There is also no defensible cutoff — two places admits 29.97 and
three does not, and neither number can be justified over the other.

Resolution is **two-dimensional**. A named size is a bounding box, not a point
on a scale: `{ max = "4k" }` means the frame fits inside 3840x2160, with axes
swapped for portrait. Anamorphic and portrait sources break any one-dimensional
ranking, so names expand to boxes and then compare numerically. The explicit
form `{ max = { width = 3840, height = 2160 } }` is always legal.

### Rejected spellings

- **Expression strings** (`">=24"`, `"24..60"`) — lose TOML types, require a
  parser and its error messages, and collide with real values.
- **Dash strings** — `"-8"` reads as minus eight.
- **`exact` as a keyword** — only needed to rescue an implicit-ceiling rule.
- **`codecs = "common"`** — a preset name in the scalar slot is a fourth
  constructor. Omit the field for the muxable set, or write
  `{ preset = "common" }`.
- **`max_tracks` as a parallel key** — sibling min/max under another name.

### Layering

There are two layers and **one** way to move between them. `[accept]` is the
default for every publisher. An admission response may select a different
named policy, and that is all it may do:

```toml
[accept.policy.premium]
video = { resolution = { max = "4k" }, frame_rate = { max = 60 } }
audio = { channels = { max = 6 } }
```

`{"policy": "premium"}` selects it. A policy **replaces `[accept]` wholesale**
for that publisher: it does not inherit the top-level tables, so anything it
leaves unsaid takes the compiled default rather than the file's value. Each
entry holds the same schema as `[accept]` itself, including the pacing bounds,
and is resolved at startup — so a name no policy defines fails the node, not
the publisher, and admission costs a map lookup rather than a parse.

Omitting `policy` applies `[accept]`, which is the common case.

**A response cannot carry predicates inline.** An earlier draft let it send an
`accept` object that overrode the file per table. Both exist to answer one
question — what may this publisher send — and offering two answers means every
deployment has to decide which it uses, while anyone reading a node's
configuration has to consult the auth service's source to know what actually
applies. A named policy keeps the whole admissible set in the file, where it
can be reviewed, diffed, and validated at startup; the response chooses among
sets rather than defining one.

That an inline object could express something a policy cannot is not a real
advantage: a per-account rule still comes from a finite set the operator
decided on, and enumerating that set is what makes it auditable. A deployment
that genuinely needs a new shape adds a policy and restarts, which is the same
cost as every other change to what this node accepts.

A policy may **widen as well as narrow**. One that could only tighten cannot
say "this account may publish 4K" on a node defaulting to 1080p, which is an
ordinary tenant rule; requiring it to be expressible would push every
deployment into a permissive base and make the base meaningless. Widening is
safe here in a way an inline override was not, because the widened set is
still one the operator wrote down.

This supersedes an earlier rule that a response could name only a policy and
never carry one — which is, in the end, where this lands again, for a
different reason. That rule was defensive, assuming a semi-trusted sidecar.
This one is about legibility: the file stays the whole truth about what the
node accepts.

There is deliberately no per-app or per-path layer. RTMP has a path namespace
and SRT's compact stream id does not, so a path-keyed table would need a story
for app-less publishers that means nothing to half of them. Per-tenant rules
belong on the auth service, which already knows the account and the stream.

## Publish speed

`ceiling`, `stall`, and `floor` are the three pacing bounds, kept side by
side so their kinship reads at a glance. `ceiling` throttles, `stall` and
`floor` disconnect — that difference in enforcement is why ceiling and floor
stay two objects rather than one list. All three are per-publisher contracts,
so a named policy may set any of them per account.

```toml
ceiling = { pace = "1x", burst = "10s" }     # at most realtime, ten-second head start
stall   = "12s"                                # nothing usable for 12s: dropped
# floor  = { pace = "0.5x", window = "30s" }  # below half realtime across 30s: dropped
```

`burst` permits media to run ahead of wall clock. With `--accept-ceiling-pace 1x`,
omitting `--accept-ceiling-burst` gives no additional head start after pre-roll.
Pre-roll still collects the media needed for timeline calibration and segmentation.
A live 1x encoder does not need a burst.

The ceiling uses media deadlines. Time spent processing media reduces the next
wait, including when `burst = "0s"`. At 1x, a healthy source produces approximately
one second of output media per second, provided the node can process it fast enough.
This controls timeline progress; individual parts can still have scheduling jitter.

After a stall, the first overdue sample is admitted immediately. Following samples
can catch up by at most `burst`. The publisher cannot save the entire idle period
and use it to send an unlimited backlog.

Exceeding the ceiling **waits**. Transport backpressure is the entire
enforcement: a publisher cannot dump unbounded media into the process, and a
file pushed at 100x still plays, slowed to live — *when a ceiling is set*.
Omitting `ceiling` is the compiled default and means exactly what it says: a
file pushed as fast as the link allows is packaged as fast as it arrives, and
plays as fast-forward. That is taken to be what an operator asked for by
setting no limit, which is why the starter file sets none either. There is no
disconnect-on-too-fast setting today, because refusing turns an encoder
catch-up or a large group-of-pictures into an outage. If one ever lands it
belongs here as a third bound — same `{ pace, window }` shape, disconnect
above — metered on pace *offered* before the ceiling throttles it, since the
ceiling masks the signal downstream. Reserved name: `cutoff`.

`floor` is `{ pace, window }`: the minimum media-time progress against
wall-time, averaged over the window. Where `stall` asks did anything usable
arrive, `floor` asks did enough of it arrive — a publisher averaging below
`pace` across any `window` is disconnected. Both fields are required once a
floor is set, including on the command line (`--accept-floor-pace` and
`--accept-floor-window`). Omitted means no floor, which is the compiled
default. The first window is startup grace, and discontinuities neither credit
nor reset progress; only discontinuity-corrected media-time counts.

Omitting `floor` means nothing *ends* a slow session — it does not mean
nothing notices. A publisher whose media time falls below 90% of wall clock is
logged as behind realtime, and logged again when it recovers past 95%. This
reports and never enforces: it is how an operator with no floor learns that a
nominally live stream is running at a quarter speed, without this node
inventing a threshold that disconnects. A publisher held at its `ceiling` is
never reported, because it is complying with an instruction this node gave it.

`stall` is **"nothing usable arrived for this long"** — no packets, or packets
that do not become media. It is explicitly *not* lag against wall clock, and
it stays idle-based on purpose: it is the fast dead-versus-alive signal, and
it resets on every usable arrival with no false-positive mode. A stable 0.98x
publisher trips no idle timer, and only trips a floor whose `pace` the
operator set above it. The two are different failure modes on different
timescales — seconds of silence versus tens of seconds of slowness — which is
why they stay two knobs rather than one list. `stall` lives here and not
under `[hls]` because it measures the publisher, not the playlist: when it
fires the origin drops the session and the outputs render the consequence
(a stale, then ended playlist). A future output table inherits the same
signal rather than growing its own timer.

`takeover` decides what a second publisher for the same stream means. At `false`,
the default, the newcomer is refused while the current publisher holds the name.
At `true`, the newcomer replaces it: the old session is closed and viewers see
a discontinuity at the join. The default is refusal because silent replacement
turns an encoder reconnect or a leaked credential into a hijack with no signal.

The cost of that default is a reconnect blackout. A publisher whose network
drops without closing its socket still holds the name until `stall` fires, so
an encoder returning before then is refused — at `stall = "12s"`, up to twelve
seconds of dead air on every partition. Which way to err is a judgement about
the deployment: `takeover = true` favours reconnect speed and accepts that
anyone with the credential can seize a live stream, while the default favours
holding the name and accepts the gap. Operators keeping the default should
size `stall` with this in mind, since it is what bounds the blackout.

`ceiling`, `stall`, `floor`, and `takeover` deliberately do **not** take the
predicate constructors above. Those answer "which values are in the admit
set"; a refill rate is not a value to test membership against. "Exactly 1x"
is not something a real encoder can be asked for.

The one argument against throttle-only that survives: pacing a pre-recorded
file makes it succeed *as live*. For an auction or a match that is the failure
case. The position taken here is that "this must be genuinely live" is
knowledge the auth service has and the origin does not.

## HLS

The `segment` and `part` objects describe preferred targets and admission limits:

```toml
[hls]
segment = { target = "6s", max = "2x", jitter = "0s" }
part    = { target = "1s", max = "2x" }
```

These are the defaults. Omitted objects and fields use their defaults.
Equivalent `[hls.segment]` and `[hls.part]` tables are also supported.
`target` accepts an absolute duration. `max` accepts an absolute duration or
an exact multiplier of the configured target, including fractional values such as `"1.5x"`.
Equal target and maximum values forbid growth during admission.

Admission selects the latest feasible common video boundary at or before the
segment target. If none fits, admission searches beyond the target up to the maximum.
It checks audio rounding and part feasibility before it selects a boundary.
Audio-only input uses encoded audio boundaries. Pre-roll capacity limits the
search resources, independently of these duration limits.

`segment.jitter` permits equal early and late movement around the admitted
runtime schedule. It accepts a duration or a multiplier of `segment.target`.
The default is zero. A 100 ms jitter allowance can increase a segment by
200 ms because both endpoints can move. The complete segment ceiling,
including audio rounding and timestamp quantization, must fit `segment.max`.

Admission freezes each selected part target. Ordinary dependent parts span
85–100% of that target; independent and final parts can be shorter.
The part writer uses bounded lookahead to repair unpublished cuts.
It never changes published parts or enlarges targets during runtime.
A later input change that cannot fit the contract terminates the publication.

This configuration replaces scalar `segment` and `part` values and removes
`admission`, `maximum_segment`, `maximum_part`, `early_boundary`, and `late_boundary`.
Old TOML forms and CLI flags are rejected. New environment variables use names
such as `RUSHLS_HLS_SEGMENT_TARGET`; CLI flags use `--hls-segment-target`.

`retain` controls how long media stays fetchable, during publication and after
the publisher disconnects. It also controls the history that the live playlist advertises.

There is deliberately no separate `playlist` window. Media a playlist does not
name is media no player can ask for, so retaining beyond the advertised window
produces bytes reachable only by a consumer that already holds the URLs — which
a player never does. The one durable exception is a segment that has just
*left* the window while a client is still fetching it, and that is a grace
period measured in seconds, derived from the cadence rather than configured.

An earlier draft split them so that a long history could sit behind a short
advertised window, on the grounds that advertising hours of segment lines is
expensive to re-send on every reload. That reasoning describes a client
re-fetching a whole playlist, which is what Playlist Delta Updates exist to
stop: with `CAN-SKIP-UNTIL`, a reload costs what changed rather than what the
window holds. Once deltas ship, the cost that motivated splitting is gone, and
a split would only mean holding media nobody can name.

The skip boundary is not the reason to split either. The specification fixes
it at **at least six target durations**, and it bounds what a delta may *omit
from one response*, not what the playlist advertises: a client without a prior
copy still receives the full window. So the boundary is a property of the
delta mechanism, derived from `segment`, and never an operator setting.

`retain` must cover three maximum target durations. A shorter fixed duration
is a configuration error. Relative values resolve against the admitted playlist target.

`retain` is **time only**, never bytes. It is a promise to viewers about how
far back a playlist can point, and viewers seek along time. The storage tiers
are the cost backing that promise, and are therefore **bytes only**. The
asymmetry is deliberate: `retain` is what you promise, the tiers are what you
spend, and the two are different questions.

A duration form on the tiers was considered and dropped. "Hold 30 seconds of
memory" has no fixed byte meaning at a variable bitrate, so it would be a
second, weaker way of writing `retain` — and the number an operator needs for
capacity planning is the one that multiplies by `streams`.

### `scrubbing`

`scrubbing = true` is the default. Set it to `false` to disable keyframe playlists.
When enabled, the multivariant playlist
advertises an `EXT-X-I-FRAME-STREAM-INF` entry for each CMAF video rendition.
Each entry references `<rendition>/iframe.m3u8`.
Audio and subtitle renditions have no I-frame playlist.

The I-frame playlist selects every available keyframe in each completed segment.
Each keyframe has its own duration, byte range, and media sequence number.
At 24 fps, a GOP of 48 provides one keyframe every two seconds.
Use a GOP of 24 for the recommended one-frame-per-second density.
It uses byte ranges into existing segments and the same initialization sections.
Its advertised bandwidth uses measured keyframe ranges, independently of the full video bitrate.
It follows the normal live retention window, including media on disk.
A segment without an opening sync sample appears as a gap.

This feature supports live low-latency HLS. The I-frame playlist has no partial
segments or preload hints. It includes PART-INF metadata for Apple validator compatibility.
It supports blocking reloads, delta updates,
rendition reports, and token query variables.
Reports target regular playlists only, including reports from I-frame playlists.
Apple hlsreport 1.20.7 rejects the optional reports that target I-frame playlists.
Its server-control values match the other playlists.
See draft-pantos-hls-rfc8216bis-22, sections 3.3, 4.4.3.6, 4.4.4.9,
4.4.6.3, 6.2.4, and Appendix B.1.

The environment variable is `RUSHLS_HLS_SCRUBBING`.
The CLI parameter is `--hls-scrubbing`. A bare flag means `true`;
an explicit `--hls-scrubbing true` or `--hls-scrubbing false`
also works.

### `hold_back`

`hold_back` is how far behind the live edge a player is told to start, and it
is therefore **the floor on live-edge latency**. It takes either a multiple of
`part` (`"3x"`, the default) or an absolute duration (`"3s"`), the same two
forms `retain` accepts.

It is exposed while the other delivery timing values stay compiled because it
is the only one that is a genuine tradeoff rather than a correctness
constraint. Below roughly three part durations a client on a lossy link runs
out of buffered parts and stalls; above it, every viewer waits longer than
they need to. Which side to err on depends on the audience's network, which
the origin cannot know. The segment-level `HOLD-BACK` stays derived at three
target durations, where the specification leaves no such latitude.

The multiple form is the default because the quantity it bounds is the part
cadence itself: an absolute value chosen against `part = { target = "1s" }` silently
becomes aggressive when parts are retuned, which is the same reasoning the
stall rules use.

There are two thresholds, because HLS has two. A value below **two part
durations** is refused: the specification requires at least twice the part
target, so this asks for a latency the protocol cannot deliver, and honouring
it approximately would advertise a promise that makes players stall. A value
between two and three parts is **warned and then honoured**: three is a
recommendation rather than a requirement, and a deployment on a controlled
network may want the lower latency knowingly.

The split matters because the two failures are different. Below two, the
configuration is invalid and no approximation is faithful to it. Between two
and three it is valid, unusual, and possibly deliberate — so it is applied as
written, with the tradeoff named. Silently raising it would be the one
response that ignores an unambiguous instruction.

The blocking-reload deadline is **derived from this value**, not configured
beside it. A client asked to sit `hold_back` behind the edge will request a
part that far ahead, so a reload deadline shorter than the hold-back would
expire on requests the origin itself told the client to make. The deadline is
`max(3 x target, hold_back + part)`, which keeps the protocol's own floor
while guaranteeing any request the advertised hold-back invites can be
satisfied.

## Storage tiers

```
memory_per_stream   the hot window
disk_per_stream     overflow, same lifetime
```

One ordered pipeline, not two pools. New media lands in memory; the oldest
moves to disk when memory is full; the oldest overall is dropped when both are
full. Anything past `retain` is dropped regardless of tier.

Effective depth is `retain` clipped by what the tiers hold at the stream's
bitrate. At 5 Mbps, 256MiB is roughly seven minutes — so `retain = "2h"` on a
memory-only node does not deliver two hours. This is reported per stream rather
than left to arithmetic.

`dir` is optional. Omit it and spilled media goes under the platform cache
directory (`~/.cache/rushls/dvr` on Linux). Set it to pin a volume. `dir`
without `disk_per_stream` is a startup error: a path with no cap does not
activate the tier. A zero disk cap is refused the same way a zero memory cap is.

The disk window is **process-lifetime**. A restart starts empty; spilled files
are not rehydrated into the catalog. Writes are still crash-safe (temp, fsync,
rename) so a new process never serves a torn object. Gzip sidecars count toward
`disk_per_stream` — omitting them would let a text rendition blow the cap while
the metric looked healthy.

`dir` is locked exclusively at startup. A second node using the same directory
fails immediately rather than deleting a live peer's files, so cutover is
stop-then-start: the draining process keeps the lock until it exits. Two
instances on one machine therefore need distinct directories — including when
`dir` is omitted and both would otherwise share the platform cache. Crash
leftovers are reaped on the next open; a generation whose owner pid is still
alive is left alone.

## Concurrency budgets

Together with the storage tiers above, this is the `[capacity]` table: how big
a box. Everything here sizes the node and refuses new work when full; nothing
here touches a live session — that is `[accept]`'s job.

`publishers` and `streams` are separate because they answer different
questions. A publisher is an ingest session — transport, muxing, CPU. A stream
is a named presentation held in the store, live *or* still within `retain`.
Once `retain` can be hours, the two decouple hard: 64 publishers with a
two-hour retain legitimately needs hundreds of store slots.

Each refuses at its own layer, which keeps failures clean: over `publishers` is
refused at admission, over `streams` at stream creation. Neither becomes a
write failure mid-session.

**When `streams` is full, a new stream is refused rather than evicting a
retained one.** `retain` is a promise to viewers holding a playlist; breaking
it produces 404s during playback, which is invisible and unactionable. A
refused publisher is neither.

There is deliberately no viewer cap. Viewers hold no ingest resources; a parked
playlist reload is a held HTTP request bounded by `shutdown` on the way out,
and fan-out is the layer in front of this node to own. Capping viewers here
would turn a CDN sizing answer into an origin config field.

### What the node actually costs

`memory_per_stream` budgets retained media and cached manifests together.
Memory-only streams give media priority and use remaining space for manifests.
With disk storage, one eighth of the RAM budget is reserved for plain and gzip
manifest bytes. Media spilling starts above 81.25% of the budget and aims for 75%.
The gap leaves room for incoming media while disk writes finish.
For a 256 MiB budget, manifests get 32 MiB; spilling starts above 208 MiB and targets 192 MiB.
These thresholds also apply when no viewer has requested a manifest yet.

The cache cannot grow beyond its allowance on disk-backed streams.
It evicts obsolete versions first, then the least recently accessed entries.
A manifest that cannot fit is served without caching. The cache holds current
playlist forms, including their DVR history, rather than past playlist versions.
Moving media to disk does not invalidate manifests; changing playlist content does.
Only media spills to disk.

The budget excludes metadata, temporary rendering buffers, transport buffers,
and bytes held only by responses already in flight. The minimum live media
window can also exceed an undersized budget. In that case, no manifests are cached.
This setting is a retention budget, not a hard process-memory ceiling.

A publisher also holds transport framing, demux queues, an in-flight batch,
pre-roll, and container probing buffers. These have a compiled bound of
**120MiB per publisher**, held only while ingesting.

```
retention budget = streams x memory_per_stream   media and manifest caches
pipeline bound   = publishers x 120MiB            ingest only
```

These terms have different lifetimes. At the reference settings, they give
64GiB of retention budget and 7.5GiB of pipeline capacity, before the overhead above.

The pipeline figure is a compiled constant rather than a setting, because an
operator has no basis on which to choose one: it is driven by track count and
group-of-pictures structure, which belong to the publisher rather than to the
deployment, and a value chosen too low breaks discovery for multi-rendition
contributors. What is owed instead is the guarantee and a way to check it, so
it is exported per session as `rushls_session_pipeline_capacity_bytes`.

A `memory_per_publisher` cap appears commented in the reference and is not
built. Enforcing one shared budget means deciding what a stage does when
another holds the bytes it wants — failing the session kills a healthy
publisher on a transient peak, blocking turns a memory cap into a stall — and
that is a decision worth making against real numbers from the metric.

## Timeouts

`[rtmp] timeout` is the idle limit for an **established** publisher: how long
it may send nothing before its session is closed. It does not set the
handshake deadline.

The handshake gets its own, tighter limit, derived rather than configured —
the lesser of `timeout` and a few seconds. The two phases are not equivalent
and sizing them together would be wrong in one direction or the other. A
handshake covers an *unauthenticated* peer, which is the cheapest way to hold a
socket open, so it should be short. An established session covers a publisher
that has proved itself, where a tight limit drops a legitimate stream between
keyframes and turns hardening into an outage.

One operator-facing value, because the second is only ever "shorter, and
bounded by a constant". Exposing both invites the pairing that the derived form
prevents: a generous session timeout accidentally applied to unauthenticated
peers.

`[srt] timeout` is the same idea one protocol over, and stays separate rather
than folding into the RTMP one. SRT is connectionless and keeps its own
keepalive, so this bounds a protocol-level idle rather than a stalled socket
read, and the two are sized against different things. This one in particular
is **bounded below by `latency`**: a deadline inside the receiver's own
reordering window would fire on packets the transport is still legitimately
waiting for, so a `timeout` at or under `latency` is refused. Raising
`latency` for a long-haul link without raising `timeout` is the mistake that
check exists to catch.

`[moq] timeout` is the QUIC idle timeout for an established WebTransport
session, derived the same way as `[rtmp] timeout`: `"off"` still leaves
SETUP/CONNECT bounded, and a value below one second is refused. It is a
separate knob because QUIC idle is not an RTMP socket read.

Five idle-adjacent knobs, four different signals. `[rtmp] timeout`, `[srt]
timeout`, and `[moq] timeout` are transport silence — any bytes reset them —
and stay per-protocol because what counts as silence differs. `stall` is
usable-media silence, protocol-agnostic. `floor` is usable-media rate.
Collapse any of them and one failure mode loses its tuning: a dead socket wants
seconds, a degraded encoder wants tens of seconds with a pace attached.

## Shutdown

`shutdown` bounds how long a restart waits before abandoning work in progress.
It covers both things a restart can cut short: viewers parked on a blocking
playlist reload, which the delivery path deliberately holds for up to three
target durations, and hook events still queued for delivery.

One knob rather than two, because it answers one operator question and is
usually sized against an orchestrator's own grace period. Set it below that
period, or a scheduler sends a hard kill mid-drain and the graceful path buys
nothing.

It sits at the top level rather than under `[capacity]` because it is not a
capacity bound. Everything in that table sizes a box — how many of something,
how much memory. This is process lifecycle, and it belongs beside `name` as a
property of the node itself.

**`"off"` is not legal here**, unlike every other duration. Waiting
indefinitely would mean staying alive to serve retained media to viewers who
might arrive, rather than finishing work already in flight — a wait bounded by
`retain`, which can be hours. No scheduler grants that, so the setting would
promise what it cannot deliver. A terminating node has usually been removed
from its load balancer already; keeping retained media available across a
restart is another node's job, not this one's refusal to exit.

## Node identity

`name` identifies this node in its own logs and metrics, and as the producer of
hook events. It defaults to the hostname, which is right for a single origin
and wrong the moment several sit behind one load balancer and their metric
series become indistinguishable.

Deliberately one field, not two. An earlier draft had a separate CloudEvents
`source` on the hook configuration, which would have been a second name for the
same node differing only in spelling. Consumers pair the producer identity with
each event id to recognise a repeat, so several nodes sharing one name appear
as a single logical producer — which is occasionally what a deployment wants,
and is expressed by setting the same `name`.

## Recording and exports

`[record]` writes persistent local copies of completed segments. It never writes
unfinished segments or individual LL-HLS parts. DVR retention does not remove
recordings, and the origin does not serve the archive.

Each MP4 file contains its matching initialization followed by its media chunks
in order. Initialization versions stay attached to the segments they describe.
WebVTT files contain one header followed by cue data. HLS delivery still uses
`EXT-X-MAP` and does not repeat the initialization in each media response.

These files preserve the source timeline. They are separate rendition files,
not a combined audio/video movie. They contain the decoder configuration but do
not add missing random-access samples or codec preroll. Video must have a usable
random-access boundary. Opus recovery can require preceding audio for an exact
start. Recording does not transcode or repair the source.

### Filesystem destinations

`dir` accepts a filesystem path. URLs, including `file://`, are refused.
HTTP export belongs to payload hooks, which remain unimplemented.
Object-storage FUSE mounts are unsuitable because they can lack the required
filesystem operations. A separate uploader can scan completed files, upload
them, then remove them. Rotation and offload remain external.

### Pattern

The default configuration is:

```toml
[record]
dir = "/archive"
pattern = "{stream}/{publication}/{time:%Y/%m/%d}/{rendition}_{segment}.mp4"
queue_size = 128
max_pending_bytes = 268435456
```

The placeholders have these meanings:

| Placeholder | Value |
| --- | --- |
| `{stream}` | Authorized stream ID, matching hook `stream_id`, with `/` preserved |
| `{publication}` | Archive publication UUID, unique across reconnects and restarts |
| `{time:...}` | UTC segment-completion time in `strftime` format |
| `{rendition}` | Stable rendition key, with `/` preserved |
| `{segment}` | Publisher-local segment sequence, starting at zero |

The archive publication UUID is separate from the process-local hook `session_id`.
The default pattern prevents collisions after sequence numbering restarts.
Custom patterns can omit placeholders, but existing files are never overwritten.
Subtitle files replace the configured suffix with `.vtt`.
Because that replacement drops everything after the last dot of the last path
component, `{segment}` cannot be the last placeholder in one: `{rendition}.{segment}`
fails configuration validation, while `{rendition}_{segment}` and
`{rendition}.{segment}.mp4` both keep the segment in the name.
Environment interpolation runs first, so `${ARCHIVE_ROOT}` can supply `dir`.

Unknown placeholders and invalid time formats fail configuration validation.
Absolute expanded paths, empty path components, `.` and `..` are refused.
Backslashes, control characters, and the `.rushls-` temporary prefix are also
refused. Archive subdirectories cannot be symlinks.

### Atomicity and failures

The writer creates a hidden temporary file in the destination directory and
syncs its contents. It then creates the final name with an atomic hard link,
removes the temporary name, and syncs the parent directory. Unlike ordinary
rename, this commit refuses an existing destination without a race.
New subdirectory entries are also synced. A final filename means the entire
file was written. Scanners must ignore `.rushls-*.tmp` files. A crash can leave
these temporary files behind.

One background thread performs filesystem writes. `queue_size` bounds
waiting segments. `max_pending_bytes` bounds retained bytes for open
segments, queued files, and the active write. Small payloads also incur a
minimum metadata charge. An open segment has a separate 16,384-handle limit.

If the queue or byte budget is exhausted, the recorder discards the entire
affected segment and reports a recording failure. Disk errors and filename
collisions also report failures. Live delivery continues. Recording is
best-effort during overload, not a lossless admission requirement.

An invalid archive root fails startup. After publishers stop, accepted writes drain
within the remaining `[node] shutdown` budget. An expired drain reports pending
outcomes as unknown. Unsynced work does not survive a process crash.

## Hook delivery

Hooks use structured CloudEvents JSON. Subscribe to `segment.ready` to receive
`rushls.segment.ready.v1` after each completed media segment is committed to delivery.
Each rendition emits its own events, including subtitle renditions.
Initialization objects, partial segments, GAP entries, rejected writes, and superseded
writes do not emit this event.

```toml
[hook.archive]
url = "http://archive.example.internal/rushls"
events = ["segment.ready"]
```

Example event data:

```json
{
  "stream_id": "live/camera",
  "rendition_id": 0,
  "segment_id": "412",
  "media_sequence": "412",
  "publication": "1",
  "path": "/live/camera/0/segment/412.m4s",
  "initialization_path": "/live/camera/0/init/1.mp4",
  "media_start": "222480000",
  "duration": "540000",
  "timebase": {"numerator": 1, "denominator": 90000},
  "bytes": "1250000",
  "independent": true,
  "discontinuity": false
}
```

Paths are relative to the origin's HTTP root. Use the externally reachable origin
address and normal playback authorization to fetch them. Proxy prefixes must be
added by the consumer. No credentials are included. `initialization_path` is null
for formats without a separate initialization resource.

`rendition_id` and `segment_id` identify retained delivery resources, not muxer-local
identifiers. `media_sequence` identifies the HLS playlist position. `publication`
identifies the publisher generation within this retained stream. A new stream after
retirement or a process restart can reuse these identifiers.

`media_start` and `duration` are ticks in the supplied rational timebase.
All 64-bit identifiers, tick values, and byte counts are decimal strings.
`bytes` counts the uncompressed segment body, excluding initialization.
`independent` reports the stored independence flag. `discontinuity` indicates a
playlist discontinuity before this segment.

This event reports live delivery availability, not successful recording or an
independent decoding guarantee. It carries no session identity or media payload.
Fetch the initialization separately when the format requires it.

Hooks do not extend retention or pin media. A delayed notification can arrive
after its resources expire. Delivery uses the existing bounded queues, retries,
and failure reporting. Consumers must deduplicate retries by CloudEvent `id`.
Use `[record]` for filesystem recording. Binary payload hooks remain unsupported.

Segment events can produce substantial hook traffic. One rendition with six-second
segments emits approximately 600 events per hour. Short segments around GAPs
increase that rate. Subscribe only the destinations that need segment notifications.

A hook destination takes the same `client_cert`, `client_key`, and `ca`
fields as `[auth.publish]`, with the same meaning and the same in-place
rotation. Nothing about them is specific to admission: both are operator-run
services reached over a network the operator may not consider private, and
proving this origin to one while pinning its issuer in return is the same
question in both places.

Only a destination that sets one of them gets a connection pool of its own.
That is not an optimisation detail — a pooled connection shared across
destinations would present one service's certificate to another — but it does
mean the common case of several plain endpoints still reads the trust store
once and shares one cache.

## Auth

Two independent questions, two tables. `[auth.publish]` decides who may send
media; `[auth.playback]` decides who may watch it. Either may be omitted, and
omission means open.

They are shaped differently on purpose, because they are asked at different
rates. A publisher is admitted once, when it connects, so a network round trip
to a service that owns the answer is affordable. A viewer re-requests a
playlist every part duration and every segment after it, so a per-request round
trip would put an external service on the critical path of playback.

### Publish

One HTTP service receives a versioned JSON request per publisher. It returns one
of these decisions:

```json
{"decision": "allow", "stream_id": "live/camera", "principal": "account-42"}
{"decision": "allow", "stream_id": "live/camera", "principal": "account-42",
 "policy": "premium"}
{"decision": "deny", "reason": "subscription_inactive"}
```

`policy` selects a local `[accept.policy]` entry. An omitted policy uses
`[accept]`. The response cannot define media predicates or override observed
transport details. Unknown fields, unknown policies, missing identities, and
blank identities fail closed. The request version defines the response schema.

The request carries `version`, `request_id`, `protocol`, `resource`, `credential`,
and `client`. Protocol is `rtmp`, `srt`, or `moq`. Credentials use base64 so
non-UTF-8 bytes survive unchanged. A configured token supplies the HTTP
`Authorization: Bearer` header independently of the publisher credential.

The call runs inside the admission deadline and never retries. A timeout,
non-success status, malformed response, or deny refuses the publisher.
A publisher can reconnect for a new admission attempt.

Both `session.started` and `session.ended` hooks carry the same observed
`protocol`, `resource`, and `client` shape as the auth request. They also carry
the authorized `stream_id` and `principal`. Credentials never enter lifecycle
events. The auth service controls the grant, while the node controls observed
transport facts.

See [Publisher API](publisher-api.md) for complete examples and hook signature
verification.

### Playback

A signed token, verified locally. The origin holds a public key, a JWKS URL, or
a shared secret, and checks the token itself — no request leaves the node per
reload.

Exactly one key source: `public_key` for an asymmetric verifying key,
`jwks_url` for a rotating key set, or `secret` for a symmetric secret (each
with a `_file` form). Asymmetric is the better default, because the origin
never holds anything that could mint a token; a symmetric secret makes every
origin able to issue viewing rights for every other one.

The verifying key is spelled `public_key` rather than `key` because
`[http.tls] key` is a *private* key. One word for both, differing only in which
one must never be shared, is how a private key ends up pasted into the wrong
field.

`[auth.playback.claims]` is what the token must say: each key is a claim name
and each value is what that claim must equal. `iss` and `aud` are required.
Without an audience check, a token the same issuer minted for a different
service is accepted here.
Values are strings, integers, or booleans compared for exact equality; claim
names are case-sensitive. A missing claim denies, and array or object values
are never matched. `exp` is required separately; `exp` and `nbf` both honor
`leeway`. Neither needs to be listed here.

Grouping them is what makes the table extensible. A deployment that also wants
`tier = "premium"` adds one line, rather than waiting for a `tier` field to be
invented. Flat `issuer` and `audience` keys would have implied a closed set of
checks and left no room for the deployment's own.

`stream_claim` stays outside that table because it is a different kind of
setting. Every key under `claims` is a constant to compare against;
`stream_claim` names *which* claim carries the stream, and its value is compared
against the request. Putting it inside would give one entry a meaning unlike
all its siblings.

`stream_claim` names **which claim** carries the stream a token admits,
defaulting to `stream`. The claim's value is matched against the requested
stream, so a token minted for one stream cannot be replayed against another.

It is deliberately not `sub`. Overloading the subject claim would spend the one
field that carries *who the viewer is* on *what they may watch*, leaving no
identity to log or revoke. Deployments that genuinely want `sub` to carry the
stream can set `stream_claim = "sub"` and accept that trade explicitly.

The token arrives as `Authorization: Bearer <jwt>` or as `?token=<jwt>`. Both
are always accepted, with no setting to rename or disable either: the header
is preferred when the client can set headers, and the query form exists
because a browser player cannot set headers on the requests its media element
issues. An empty `?token=` is a 401, not an open playlist.

Native HLS cannot copy `Authorization` onto the URIs it fetches next, so a
query-token client is answered a **second playlist form**: protocol version 11,
`#EXT-X-DEFINE:QUERYPARAM="token"`, and every minted URI carrying
`?token={$token}`. The player substitutes the query value it already has.
Bearer and ungated clients keep the ordinary v6/v9/v10 playlists, with no
DEFINE. Both forms are gzipped once at render; the origin does not re-gzip per
viewer, and two viewers with different tokens receive identical playlist bytes
and the same `ETag`. `EXT-X-DEFINE IMPORT` is out of scope.

That form is a hard requirement on the player: a client that does not expand
QUERYPARAM (hls.js-light among them) will request `token={$token}` literally
and be refused. A CDN or player that cannot speak protocol version 11 will
hard-fail the entry playlist rather than degrade. Apple's `mediastreamvalidator`
rejects v11, so the ungated `apple_hls` path stays on the ordinary form.

When playback authorization is on, `Cache-Control` **drops `public`**. `max-age`
is unchanged. RFC 9111 § 3.5: `public` waives the default rule that a response
to a request carrying `Authorization` must not be stored by a shared cache.
Leaving it on would let a CDN serve one viewer's playlist to another.

CDN cache keys are a deployment concern this origin cannot enforce:

| Resource | Key on |
|---|---|
| Media (init, segments, parts) | Strip `token`. The bytes are identical for every admitted viewer. |
| Playlists | The **presence** of `token`, not its value. Bearer playlists and query-token playlists are different bytes; a cache that ignores the query entirely will mix them. Never ignore all query parameters: `_HLS_msn` / `_HLS_part` / `_HLS_skip` name different playlists. |
| Redirects | Must keep `token` on the Location they issue. A 302 that drops it is a 401 on the next hop. |

The query form has a consequence that is a requirement rather than advice:
**tokens appear in access logs, referrer headers, and any intermediary's
request log.** Tokens must therefore be short-lived and scoped to a single
stream, and the origin redacts `token` from its own access logging.

**Hook `url` values are not exempt.** When playback authorization is on, the
segment URL in a structured-mode event is subject to the same check as any
other request, so a consumer needs a token of its own. A consumer that should
not hold one uses `payload = true` and receives the bytes directly.

## Metrics

**Omit `[metrics]` and nothing is served.** The table is the switch, as it is
for `[auth.publish]` and `[http.tls]`.

An earlier draft had both an `enabled` flag and a `listen` that accepted
`"off"`, which is two disable switches meaning different things: one turning
metrics off entirely, one moving them to a shared port. Table-as-switch removes
the ambiguity and matches how every other optional feature here reads.

Present, metrics get their **own listener**, defaulting to loopback. Prometheus
series carry stream names, which on a public origin is the list of everything
currently published — not something the viewer-facing port should offer.

To serve them on a viewer port instead, set `listen` to the same address as
`[http]` or `[http.tls]`. Sharing `[http.tls]` is how scrapes happen over
HTTPS; a dedicated metrics listener is always cleartext. That is a deliberate
act rather than a magic value, and it makes the sharing visible in the file.
A token then stops being optional in any public configuration.

Two scrape paths: `/metrics` for process totals and hook series,
`/metrics/streams` for anything labelled by a stream or session. The scraper
chooses, so unbounded cardinality is a second Prometheus job rather than a
node-side flag.

`/metrics/streams` includes live session and input-track progress, per-rendition
output deadlines and wall-time lag, rendition comparisons, and retention.
`/metrics` contains node totals, HTTP body and response measurements, operation
durations, capacity, and hook delivery measurements.

See [Metrics](metrics.md) for exact semantics, replacement names, queries,
a Grafana dashboard, alert rules, and the public-playback probe.


## Secrets

Every secret has two spellings: a `_file` form reading from a path, and an
inline form. The `_file` form is preferred and is what the reference shows
uncommented, because a mounted file with restricted permissions does not appear
in process listings, crash dumps, or a configuration file that gets committed
by accident.

The inline form exists because not every deployment has somewhere to mount a
file, and because it composes with interpolation:

```toml
secret = "${PLAYBACK_HMAC}"
```

**`${VAR}` is the canonical spelling**, with unambiguous boundaries so a value
can be composed from more than one variable. Interpolation applies to string
values only, never to table names or keys.

`$$` escapes a literal `$`. This matters: passphrases and publishing keys can
legitimately contain one, and silently interpolating those would corrupt a
working credential.

**An undefined variable is an error, not an empty string.** Substituting `""`
into a token would silently disable the check it was protecting. Where an empty
value is genuinely wanted, `${VAR:-fallback}` says so explicitly.

Setting both forms of one secret is refused rather than resolved by precedence.

The `_file` path is a setting like any other: file, environment, or a
command-line flag (`--auth-publish-token-file`, `--srt-passphrase-file`,
`--metrics-token-file`). The secret itself is not a flag. A process listing
would otherwise show it. Environment and the file remain the two places an
inline value may appear. Hook tokens stay in `[hook.*]` only: that table is an
open namespace, and there is no flag for a name chosen at runtime.

## Configuration mechanics

Both Rushls and Routmp use `cc-config` for source loading, interpolation, and
file discovery. Unknown `RUSHLS_` environment variables produce startup warnings
and are ignored as configuration overrides. Warnings include names, never values.
Variables outside that prefix produce no warning. All environment variables remain
available for interpolation. Custom interpolation inputs can use a separate
namespace to avoid warnings.

Mounted secret files lose trailing CR/LF characters only. Leading and trailing
spaces remain part of the credential. Inline credentials remain unchanged.


Unknown keys refuse. A misspelled table or field fails startup with its path,
because silently ignoring it would run an open node the operator thought was
closed. This is the same fail-closed instinct as an unknown policy name.

**Two tables are open namespaces**, and are the only exceptions: `[hook.*]`
and `[accept.policy.*]`, whose sub-table names the operator chooses. The names
are keys rather than values — a hook's name identifies it in logs and metrics,
and a policy's is what an auth response selects — so a table keyed by name
makes uniqueness structural, since TOML rejects a duplicate key for us. The
alternative spelling, an array of tables with a `name` field, turns
uniqueness into a validation rule that has to be written and can be forgotten,
and demotes the name from the heading to a line inside the block, which reads
worse in a file built to be skimmed. Unknown keys *within* one of these tables
still refuse.

Precedence of **values** is file, then environment, then command line; later
wins. Environment uses the existing `RUSHLS_` names, so containers can inject
secrets without rewriting the file. Interpolation runs on file values before
overrides apply, so `${VAR}` in the file and a `RUSHLS_` override compose
rather than compete.

Which **file** is loaded is a separate walk, first match wins:

1. `--config` (fails if the path is missing)
2. `RUSHLS_CONFIG` (same)
3. `./rushls.toml` in the working directory
4. the platform local config directory (`rushls.toml`)
5. the platform config directory (`rushls.toml`)

The process logs the path it used, or that it used compiled defaults.

An environment value is **a TOML fragment**, not a second grammar. A scalar
needs no ceremony, because a bare scalar is already valid TOML on the
right-hand side:

```
RUSHLS_HLS_SEGMENT_TARGET=6s
RUSHLS_ACCEPT_VIDEO_FRAME_RATE='{ min = 24, max = 60 }'
```

Structured predicates therefore stay overridable, which a scalars-only rule
would have prevented for much of `[accept]`, and they are parsed by the same
code that parses the file, so a predicate means exactly one thing in both
places. JSON was the alternative and is rejected for being a second grammar
for values the file already spells.

The file is read at startup. New certificates and rotated JWKS keys take effect
without a restart; anything else requires one. There is deliberately no reload
signal for `accept`, `capacity`, or `hls`: retuning cadence or capacity under a
live edge is a restart, sized by `shutdown`. `rushls check` validates the file,
its directories, and its secret paths without booting.

## What "off" and omission mean

Omission is how a whole feature is turned off; `"off"` is how a single limit
is lifted.

- `[auth.publish]` omitted — anyone may publish.
- `[auth.playback]` omitted — anyone with the URL may watch.
- `[metrics]` omitted — nothing is exported.
- `[record]` omitted — no local copies are written.
- `[http.tls]` omitted — no HTTPS listener.
- `[moq]` omitted, or `listen = "off"` — no WebTransport ingest. The compiled
  default; turning it on requires a certificate and key.
- `[capacity]` omitted — no stream count cap, though the compiled per-stream byte
  backstop remains.

- `[http] listen = "off"` — no cleartext listener. Both listeners off refuses to
  boot.
- `ceiling` omitted — no publish rate ceiling. This is the compiled default.
- `stall`, `[rtmp] timeout`, `[moq] timeout`, and every numeric `[accept]` predicate accept `"off"` to
  mean no cap. Omitting a predicate already admits everything; `codecs = "off"` is
  refused rather than given a second spelling for omission.
- `memory_per_stream = "off"` — the explicit "fill memory", and one of the
  conditions the public-bind warning keys off.

`shutdown` is the one duration with no `"off"`, for the reasons above.

### Shipped files

Two files, and the file is the stance:

- `rushls.toml` — the local starter. Loopback listeners, everything else
  compiled defaults. Push a file, watch it play.
- `rushls.reference.toml` — the complete surface, hardened for a public origin.
  Copy this one to deploy.

The container image ships the reference, so a default container is not an open
origin.

## Startup warnings

Legal but probably unintended, warned rather than refused: an ingest listener
on a non-loopback address combined with open auth, an uncapped stream count,
`memory_per_stream = "off"`, or an omitted `ceiling`. A disabled RTMP or MOQ
timeout on a bound listener is a second warning.

Hard refusals are kept for the genuinely unbootable. Rules that bounce an
administrator over a relationship they did not know existed should warn.

## Deliberately not included

- A metrics `per_stream` flag. `/metrics` versus `/metrics/streams` is the
  scraper's choice.
- Simultaneous HTTP and HTTPS metrics. One `listen`, one transport. Share
  `[http]` or `[http.tls]`, or bind a dedicated cleartext port.
- Per-path or per-app accept maps — named policies cover it.
- A disconnect-on-too-fast setting.
- Classic HLS, or a `low_latency` flag. The origin is low-latency HLS.
- `log_level` — `RUST_LOG` already does this.
- Configurable health probe paths.
- Per-hook queue depth, retry counts, and response ceilings.
- Per-hook rendition filtering — `ce-rendition` lets a consumer filter on
  receipt, and rendition names are not knowable in advance while the origin is
  pass-through.

## Known future pressure

Additive, no reorganization needed: a `[dash]` sibling to `[hls]`, stream-key
auth as `[auth] key_file`, further ingest protocols as new top-level tables,
and `EVENT`-type playlists, which would arrive as a new `[hls]` field naming
the playlist type rather than as a second retention window.

Structural, and honestly not designed for: **transcoding**. A rendition ladder
needs named variants and per-variant constraints, which is a new top-level
concept rather than a field.

## CORS response headers

`http.cors.expose_headers` sets the response headers that cross-origin players can read.
TOML accepts an array of header names:

```toml
[http.cors]
expose_headers = ["content-length", "content-range", "date"]
```

These are the defaults. An explicit array replaces the list; `[]` leaves only browser-safelisted headers readable.
Invalid names and `*` stop startup. This keeps exposure explicit for credentialed requests.
The setting has no effect when CORS is off.

Use comma-separated values with `RUSHLS_HTTP_CORS_EXPOSE_HEADERS` or `--http-cors-expose-headers` to override the file.
An empty CLI or environment value clears the list.
For example, `--http-cors-expose-headers content-length,content-range,date,x-request-id` exposes an additional response header.
This setting exposes names; it does not create those response headers.

Keep CDN-specific exposure in the CDN response-header policy when the CDN owns those headers.
Preserve the origin exposure list when you add CDN header names.

Audio gaps use [bounded GAP handling](audio-recovery.md), enabled by default. Select permissive or strict behavior with `[accept].strict`. See [input modes](input-modes.md).
