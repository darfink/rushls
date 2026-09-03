# RFC: the Rushls configuration surface

> **Status: proposed.** This is a design under review, not a description of
> what the application does today. Nothing here is implemented.
>
> **The goal is inverted on purpose.** This document does not describe how to
> configure the current internals. It describes the configuration an
> administrator would want if the internals did not exist yet — and the
> application is then expected to change to serve it. Where this document and
> the code disagree, the code is what moves.
>
> Replaces the earlier open discussion in `CONFIG.md`, now removed. That
> document contradicted this design in several places.

The proposed surface is [rushls.reference.toml](../rushls.reference.toml).
That file is the contract. This document is why it is shaped that way, and it
records the promises the file makes but cannot explain in a one-line comment.

## What this is reacting to

The current `rushls.toml` is a complete reference — every knob, every default,
an essay per field — pretending to be the file a new administrator opens.
Three consequences:

1. **The altitude is flat.** `listen` sits beside `publication_stall_multiplier`
   and hook `queue_capacity`.
2. **Limits are a scavenger hunt.** Hardening an untrusted edge means touching
   six tables, and several caps have no "unlimited" form.
3. **The file speaks pipeline.** `faster_than_realtime`, `maximum_lead`,
   `maximum_timestamp_jump`, `source_stall` vs `media_stall`,
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

All binds default to dual-stack `[::]`. `public_url` only shapes playlist URLs:
empty means relative, which is right behind a proxy or CDN; a trailing slash
is insignificant. Certificates reload in place on rotation, with secure TLS
defaults and no cipher knobs. The usual proxy posture is cleartext on loopback
with TLS ended in front, rather than TLS in this process. No trusted-proxy
list is configured; header trust is not how playlist URLs or auth are derived.

## `[accept]`

`[accept]` is admission: everything about whether a publisher is let in and on what
terms. The top level holds admission controls (`speed`, `burst`, `takeover`,
covered under Publish speed below); the nested `[accept.video]`, `[accept.audio]`,
and `[accept.subtitles]` tables hold predicates.

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
and `30000/1001` are the same rate and both are accepted, neither rounded
before comparison. `frame_rate = 30` therefore does not admit 29.97 — the
predicate doing what it says — and the range form is how a deployment accepts
both.

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

Two layers, same schema, later wins:

1. `[accept]` and its nested tables — the default for every publisher.
2. The auth response’s `accept` object — an optional partial overlay.

Only listed keys override. An overlay of
`{"video": {"resolution": {"max": "1080p"}}}` leaves `video.frame_rate` and the audio predicates untouched.
Unknown keys in an overlay deny the publisher, fail-closed.

There is deliberately no per-app or per-path layer. RTMP has a path namespace
and SRT's compact stream id does not, so a path-keyed table would need a story
for app-less publishers that means nothing to half of them. Per-tenant rules
belong on the auth service, which already knows the account and the stream.

## Publish speed

`speed` is a **ceiling**, expressed as a multiple of realtime, plus `burst` for
how far ahead of that rate a publisher may get.

```toml
speed = "1x"     # at most realtime
burst = "10s"    # with a ten-second head start
```

Exceeding it **waits**. Transport backpressure is the entire enforcement: a
publisher cannot dump unbounded media into the process, and a file pushed at
100x still plays, slowed to live. There is no disconnect-on-too-fast setting,
because refusing turns an encoder catch-up or a large group-of-pictures into an
outage.

`takeover` decides what a second publisher for the same stream means. At `false`,
the default, the newcomer is refused while the current publisher holds the name.
At `true`, the newcomer replaces it: the old session is closed and viewers see
a discontinuity at the join. The default is refusal because silent replacement
turns an encoder reconnect or a leaked credential into a hijack with no signal.

`speed`, `burst`, and `takeover` deliberately do **not** take the predicate
constructors above. Those answer "which values are in the admit set"; a refill
rate is not a value to test membership against. "Exactly 1x" is not something
a real encoder can be asked for.

The one argument against throttle-only that survives: pacing a pre-recorded
file makes it succeed *as live*. For an auction or a match that is the failure
case. The position taken here is that "this must be genuinely live" is
knowledge the auth service has and the origin does not.

## HLS

`segment` and `part` are output cadence. `retain` is **how long media stays
fetchable** — while live, and after the publisher drops. `playlist` is how much
the live playlist advertises, defaulting to `min("1m", retain)`.

Splitting them is what makes long history usable: `retain = "2h"` with
`playlist = "1m"` gives depth without making every player re-fetch hours of
segment lines on each reload.

Both have a floor of **three segments**, which live playlists require. A
`retain` or `playlist` below that is raised to it with a warning rather than
refused: the intent is unambiguous and refusing to boot over an arithmetic
relationship an operator did not know about is the nagging this design avoids.

`retain` is **time only**, never bytes. It is a promise to viewers about how
far back a playlist can point, and viewers seek along time. The storage tiers
are the cost backing that promise, and are therefore **bytes only**. The
asymmetry is deliberate: `retain` is what you promise, the tiers are what you
spend, and the two are different questions.

A duration form on the tiers was considered and dropped. "Hold 30 seconds of
memory" has no fixed byte meaning at a variable bitrate, so it would be a
second, weaker way of writing `retain` — and the number an operator needs for
capacity planning is the one that multiplies by `streams`.

`stall` is **"nothing usable arrived for this long"** — no packets, or packets
that do not become media. It is explicitly *not* lag against wall clock. A
behind-ness measure was proposed and rejected: a publisher at 0.98x realtime
accumulates drift forever and would be disconnected on a timer while sending
perfectly good media. Idle-based detection resets on every arrival and has no
false-positive mode.

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

`dir` is required when `disk_per_stream` is set. Startup fails otherwise,
rather than silently falling back to memory-only.

## Concurrency budgets

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

## Shutdown

`shutdown` bounds how long a restart waits before abandoning work in progress.
It covers both things a restart can cut short: viewers parked on a blocking
playlist reload, which the delivery path deliberately holds for up to three
target durations, and hook events still queued for delivery.

One knob rather than two, because it answers one operator question and is
usually sized against an orchestrator's own grace period. Set it below that
period, or a scheduler sends a hard kill mid-drain and the graceful path buys
nothing.

It sits at the top level rather than under `[limits]` because it is not a
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

## Exports carry their own header

A fragmented-MP4 segment cannot be decoded without its initialization segment.
The rule:

- **Segments leaving the origin carry their header.** Both `[record]` files and
  hook payloads are self-contained and play on their own.
- **Segments served over HLS do not.** `EXT-X-MAP` is how the specification
  wants it, and duplicating the header to every viewer is real bandwidth.

A header is typically under 2KB against a multi-megabyte segment — roughly
0.04% — and it removes a whole class of silent corruption, because an
initialization can be retired while segments that referenced it are still live.
A consumer caching "the header for this rendition" can otherwise pair the wrong
one after a mid-stream parameter change.

## `[record]` versus hooks

| | Moves | Destination |
| --- | --- | --- |
| `[record]` | segment bytes | a filesystem |
| `[hook.*]` | events, optionally with bytes | HTTP |

Every outbound HTTP request is a hook. `[record]` writes locally and has no URL
form, because sending bytes over HTTP is what a hook with `payload = true`
already does, and two ways to POST identical bytes to identical endpoints is
the kind of silent overlap this design removes elsewhere.

`[auth]` keeps its own table despite also being HTTP: it is request/response
inside the publisher's admission deadline and its answer changes what happens
next. Hooks are fire-and-forget with retries and a queue. Same transport,
opposite control flow.

### Pattern

`pattern` has four variables. `{stream}` is the requested name with `/` preserved
as directories; `..`, absolute paths, and empty segments are refused rather
than sanitized into collisions. `{time:...}` is the UTC wall-clock time of
segment completion in `strftime` form. `{rendition}` is the durable rendition
name and `{segment}` is its integer media sequence. The trailing extension in the
pattern sets the media suffix (`.m4s` in the reference); subtitle renditions keep
the same basename with `.vtt` instead, so one pattern covers both without collision.
Unknown variables refuse at startup. The directory grows unbounded by design;
rotation and offload are external, which is why the atomic-rename contract below matters.

### Atomicity

Recorded segments appear at their final name only once fully written: write to
a temporary name on the same filesystem, sync, rename, then sync the parent
directory. Rename within a filesystem is atomic, so a directory scan never sees
a partial file.

That gives a colocated uploader a contract with no coordination: scan, upload,
delete. A file existing at its final name *is* the completion signal, which
survives a restart in a way that a missed notification does not.

Object-storage FUSE layers do not provide atomic rename. For those targets the
right answer is a hook with `payload = true`, not `[record]` onto a filesystem
that only looks like one.

Completed segments only, never low-latency parts. The archive therefore lags
the live edge by up to one `segment`.

## Hook delivery

Bodies are CloudEvents. Two modes:

In configuration you subscribe with the short name (`segment.ready`). On the wire
the type is versioned (`rushls.segment.ready.v1`) so consumers can distinguish
breaking changes. A subscription receives all versions of that event.

**Structured** (`payload = false`, the default) — the whole event as JSON,
including a `url` for the segment, fetchable while within `retain`.

```http
POST /rushls
Content-Type: application/cloudevents+json

{"specversion": "1.0", "id": "...", "source": "origin-1",
 "type": "rushls.segment.ready.v1", "subject": "live/camera",
 "time": "...", "data": {"rendition": "video-1080p", "segment": 412, ...}}
```

**Binary** (`payload = true`) — context attributes become headers and the body
is the segment itself.

```http
POST /rushls
Content-Type: video/mp4
ce-specversion: 1.0
ce-id: 01JAV9F2K3QX
ce-source: origin-1
ce-type: rushls.segment.ready.v1
ce-subject: live/camera
ce-time: 2026-09-03T10:14:22.481Z
ce-rendition: video-1080p
ce-segment: 412
ce-duration: 6006
ce-discontinuity: false

<segment bytes, header prepended>
```

Four extension attributes, and no more: enough to name the file, order it, and
rebuild a playlist. `ce-discontinuity` matters because moderate timestamp jumps
become discontinuities rather than session failures, and an archive that loses
that bit cannot reconstruct a correct timeline.

Constraints the specification imposes, worth stating because they are
surprising: attribute names are lowercase alphanumeric with no separators, an
attribute may not be named `data`, there is no float type (hence duration in
milliseconds), and `ce-datacontenttype` must be absent in binary mode because
`Content-Type` carries it.

The two modes are not symmetric and should not pretend to be. Structured is the
complete event; binary is the media plus enough identity to file it.

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

One HTTP service, asked once per publisher:

```json
{"decision": "allow", "stream": "live/camera", "user": "account-42"}
{"decision": "allow", "stream": "live/camera", "user": "account-42",
 "accept": {"video": {"resolution": {"max": "1080p"}}}}
{"decision": "deny", "reason": "subscription_inactive"}
```

No policy names. The optional `accept` overlay uses the same field names as the
TOML, which is what lets a control plane express per-account limits without a
configuration change — the single largest functional gain in this design.

The request is a POST with a JSON body carrying four fields: `protocol` (`rtmp`
or `srt`), `stream` (the requested name), `remote` (the peer address), and a unique
`request_id` the service can use for idempotence. When a token is configured it
goes out as `Authorization: Bearer`. The call has a short
timeout inside the 10s admission deadline and is fail-closed: a timeout, an
error, or a deny all refuse the publisher, with no retries. A refused publisher
can reconnect, which is the retry.

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
issues.

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

To serve them on the HTTP port instead, set `listen` to the same address as
`[http]`. That is a deliberate act rather than a magic value, and it makes the
sharing visible in the file. A token then stops being optional in any public
configuration.

Two scrape paths either way: `/metrics` for totals, `/metrics/streams` for
per-stream series. The scraper chooses, so unbounded cardinality is the
caller's decision rather than a node-side flag.

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

## Configuration mechanics

Unknown keys refuse. A misspelled table or field fails startup with its path,
because silently ignoring it would run an open node the operator thought was
closed. This is the same fail-closed instinct as the auth overlay.

Precedence is file, then environment, then command line; later wins. Environment
uses the existing `RUSHLS_` names, so containers can inject secrets without
rewriting the file. Interpolation runs on file values before overrides apply, so
`${VAR}` in the file and a `RUSHLS_` override compose rather than compete.

The file is read at startup. New certificates and rotated JWKS keys take effect
without a restart; anything else requires one. There is deliberately no reload
signal for `accept`, `limits`, or `hls`: retuning cadence or capacity under a
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
- `[limits]` omitted — no stream count cap, though the compiled per-stream byte
  backstop remains.

- `[http] listen = "off"` — no cleartext listener. Both listeners off refuses to
  boot.
- `speed = "off"` — no publish rate ceiling. This is the compiled default.
- `stall`, `[rtmp] timeout`, and every numeric `[accept]` predicate accept `"off"` to
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
`memory_per_stream = "off"`, or `speed = "off"`. A disabled RTMP timeout on a
public bind is a second warning.

Hard refusals are kept for the genuinely unbootable. Rules that bounce an
administrator over a relationship they did not know existed should warn.

## Deliberately not included

- Per-path or per-app accept maps — the auth overlay covers it.
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
and `EVENT`-type playlists as a value on `playlist`.

Structural, and honestly not designed for: **transcoding**. A rendition ladder
needs named variants and per-variant constraints, which is a new top-level
concept rather than a field.

**Playback authorization is designed here but not built.** `[auth.playback]` is
specified above and appears in the reference; the application has no notion of
viewer identity yet. Designed, not deferred to a later design.
