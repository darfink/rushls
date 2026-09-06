# RFC companion: what the configuration asks the application to become

> **Status: largely implemented.** Outstanding, in the order they are least
> to most self-contained: effective retention-depth reporting,
> moderate timestamp jumps becoming discontinuities,
> payload-carrying hooks, `[record]`, delta playlists, the disk tier,
> playback authorization, and enforcing
> `memory_per_publisher`. Every key belonging to an unbuilt feature is refused
> at startup by name, so nothing here silently does nothing. Everything else
> has landed.
>
> This exists because [config.md](config.md) deliberately designs the
> configuration **without regard for what the internals currently look like**.
> That is the point of the exercise: the operator surface is chosen first, and
> the application changes to serve it. This file was the honest accounting of
> what that would cost; it is kept as the record of why each change was made.
>
> Every entry is a place where the proposed configuration describes behaviour
> the application did not have. None of them were reasons to change the
> configuration — they were the work the configuration implied.

## Why this ledger exists

The earlier design framed several of these as "reshapes", implying a
translation-layer mapping from a new name onto an existing field. Some are.
Several are not: they describe a system that would have to be built. Filing
those honestly is the difference between "config rewrite" and "config rewrite
plus five subsystem changes".

## Ingest timing

**A rate multiplier in the pacer.** `ceiling.pace = "3x"` has no
representation today. The pacer compares media-elapsed against wall-elapsed
one to one, so every multiple other than 1x is new arithmetic.

**`ceiling.burst` is not the current lead tolerance.** The existing
`maximum_lead` is a standing allowance a publisher may sit inside
indefinitely. A burst is consumable and refills at `ceiling.pace`. Better
semantics, but genuinely different behaviour rather than a rename.

**The timestamp-jump guard must detach from pacing.** It currently lives inside
one arm of the timing policy, so with no rate ceiling — the compiled default —
there is no guard at all. Since a forward jump inflates the timeline, that is
also the fastest route to exhausting a stream's byte budget.

**A moderate jump should become a discontinuity, not a session failure.** RTMP
publishers drop frames under network stress routinely; disconnecting a
publisher for recovering from a gap is the wrong response. The delivery path
already propagates a discontinuity flag into `EXT-X-DISCONTINUITY` and
`EXT-X-DISCONTINUITY-SEQUENCE`, and emits `EXT-X-PROGRAM-DATE-TIME` at the same
boundary, which is what re-anchors wall clock after a gap. Two sub-questions to
settle when building: a discontinuity must fall on a segment boundary, and
whether a fresh initialization segment is required after one.

Only an implausible jump — hours, an epoch change — should remain fatal.

**Not in the first adoption.** The intended end state, recorded so the pacing
work does not foreclose it: a small forward jump is treated as *elapsed media
time* — a three-second jump means the timeline advanced three seconds, and the
token bucket is charged accordingly rather than counting the samples that did
not arrive — while a large jump becomes a discontinuity that resets both the
timeline and the pacing state. This falls out of a token bucket charged by
media-time delta, so the initial implementation should charge that way even
while every jump above the threshold stays fatal.

The depth here is cross-track coherence. The pacer's watermark is the maximum
across all tracks, so a discontinuity has to be applied to every track at one
agreed point or they desynchronise at the splice. That, the segment-boundary
requirement, and the initialization question are why this is deferred rather
than folded in.

## Health

**`stall` stays idle-based.** No work needed, recorded because it was nearly
specified as behind-ness against wall clock, which the evaluator does not
measure and which would disconnect a slightly slow encoder on a timer. The
existing source and media idle checks collapse into one operator-facing
duration.

**The publication deadline is deleted, not re-derived.** `HealthEvaluation`
today has a third alarm beside the two idle checks: `PublicationStalled`,
which fires when nothing reaches the store for
`expected_publication_interval x stalled_publication_multiplier` measured in
wall time. It is the check that must go under `ceiling` and `floor`, because
a legitimately throttled or slow publisher trips it on a timer.

Deleting it costs one property worth naming. The three alarms sit at three
different stages — `source_progress` at demux, `media_progress` at
normalization, `delivery_progress` at the store — and pacing suppresses the
first two deliberately, because a sleeping pacer stops the loop reading input.
So while a publisher is being throttled, the publication deadline is currently
the only alarm still armed, and a muxer that consumed samples and emitted
nothing would go unnoticed without it.

That property should be recovered by **making the two surviving alarms
pacing-aware rather than pacing-suppressed**. The idle checks are suppressed
only because a pacing sleep makes their clocks meaningless; excluding time
spent sleeping from those clocks, rather than muting the checks outright,
keeps them armed throughout. A muxer that stops producing then shows up as
media silence, which is what it is, and no alarm needs to know about
wall-clock cadence at all.

A muxer accepting samples and emitting nothing is otherwise a bug rather than
an operational condition, and one that the segmentation contract — parts and
segments of a declared duration — already constrains. It does not warrant a
wall-clock deadline that a slow publisher can trip.

**What remains is one check, not three.** The two idle alarms collapse into
the single operator-facing `stall`, and what it watches is *usable media* —
the normalization signal. The demux-stage signal stops being a separate alarm:
a source delivering bytes that never become media is what `stall` is for, and
splitting it in two only ever changed which stage the message named.

That naming is worth keeping, but as an **attribution on the event**, not as a
second timer. When `stall` fires, whether bytes were still arriving is the
difference between a dead peer and a broken elementary stream, and an operator
reading "stalled" without it goes looking in the wrong layer. The counters
that answer it already exist and are already exported, so this is a field on
the event rather than machinery.

**`stall` cannot become metrics-only.** Reporting is the right home for
everything diagnostic here, but this one check terminates a session, and
nothing else does: a publisher whose socket is open and silent holds its
registration, its stream name, and a publisher slot until something ends it.
Scraped metrics inform an operator; they do not free the slot. `stall` stays
the one health signal with teeth, which is why it is also the only one the
reference exposes.

**Slowness is reported, not diagnosed here.** How far media time trails wall
time is what `floor` measures, and `floor` is the only thing that should end a
session for it. Where an operator sets no floor, drift is a fact to expose —
the meters already carry it — not a fault to invent a threshold for.

A gauge alone turned out to be too quiet for the case that matters most. A
node with no `floor` — the default — has nothing that ends a drifting session
and nothing that mentions it either, so a stream that is nominally live but
running at a quarter speed is visible only to whoever is already watching the
graph. `DriftMonitor` in the pacer closes that: it emits
`PublisherBehindRealtime` and `PublisherTrackingRealtime`, and does nothing
else. It never fails a session, which is what keeps it from becoming the
publication deadline again under a new name.

Three properties make it reportable rather than noisy. It emits on
*transitions*, so a stream an hour behind logs twice rather than on a timer.
Its thresholds have a hysteresis gap — behind at under 90% of realtime,
recovered at 95% — because a publisher hovering near realtime would otherwise
alternate every window. And it is charged the same media-time delta the
ceiling bucket is, so a publisher held at exactly its ceiling registers as
complying rather than drifting; blaming a publisher for obeying this node's own
instruction is precisely the bug that was removed.

Its window and thresholds are compiled. A knob controlling when a log line
appears is not a tradeoff an operator is better placed to make than the node,
and exposing it would re-open the question of what happens when it is crossed.

The same division applies to the stages generally: **health terminates,
metrics explain.** Per-stage progress counters stay and are what an operator
scrapes to find which layer stopped; what goes is the idea that each stage
needs its own configurable deadline. Three timers were three chances to fail a
healthy publisher, and none of them told an operator anything the counters do
not.

Net effect on `HealthEvaluation`: `SourceStalled` and `MediaStalled` become
one variant carrying the attribution, `PublicationStalled` and its multiplier
and floor go entirely, and `PacingPublisher` disappears with the suppression
it existed to express. `HealthPolicy` reduces to one duration.

## HTTP

**Two listeners.** Cleartext and TLS are currently the same socket, so enabling
TLS silently moves cleartext off its port rather than adding HTTPS beside it.
Needs a second bound address and a second task. The server itself is already
generic over the listener, so only wiring changes.

## Capacity

**Separate publisher and stream budgets, each refusing at its own layer.**
Admission refuses over the publisher budget; stream creation refuses over the
stream budget. The store's capacity must be derived with reconnect headroom so
a burst of retained streams cannot turn a newly admitted publisher into a
mid-session write failure.

**A full store refuses rather than evicting.** Retiring a retained stream early
would break a promise viewers are already holding a playlist against.

**Shed oldest media under byte pressure.** The write path currently sweeps
expired media and, if that is not enough, refuses the write — which fails the
session. Long retention windows make that the common case rather than the edge
case. The eviction machinery exists; the change is to drop oldest rather than
only time-expired media in the same retry path.

The same applies to `maximum_parts` and `maximum_segments`, which fail the
same way and are compiled constants. At `segment = "1s"` with `retain = "2h"`
a stream needs 7200 segments against a 4096 ceiling, so the session dies
part-way through with no operator setting that explains why. Shedding has to
cover all three budgets, not only bytes.

Once shedding is in, the store can no longer refuse a write for capacity, and
that is what keeps the pipeline synchronous. An asynchronous write that waited
for store capacity would couple output cadence to retention pressure, letting
one stream's eviction sweep stall another's muxer. Dropping the oldest media
removes the condition that would have needed waiting on.

**Transient pipeline memory is unaccounted for.** `memory_per_stream` bounds
retained parts and segments, which is what it should mean to an operator. It
does not bound what a publisher holds *before* the store, and that is
substantial: per RTMP session, 16MiB of buffered FLV, 16MiB queued for
AVFormat, 16MiB of in-flight batch, 64MiB of pre-roll, and 8MiB of discovery
probe — roughly 120MiB that appears nowhere in `streams x memory_per_stream`.
SRT adds its own receive buffer.

These stay compiled rather than becoming operator settings: they are
properties of the pipeline, not policy, and deriving them from
`memory_per_stream` would wrongly couple retained-window sizing to demux
buffer sizing. The work is to name the per-publisher total as one constant
that the individual buffers derive from, and to state the worst case as
`publishers x` that figure in `config.md`, so capacity planning has the
number without gaining a knob.

The individual buffers stay internal, but the **total** is an operator-facing
fact and documenting it alone is not enough. The two terms have different
lifetimes, which is the same seam `publishers` and `streams` already sit on:
retained media is charged per stream and outlives its publisher by `retain`,
while pipeline memory is charged per publisher and is released the moment
ingest stops. Worst-case memory is therefore
`streams x memory_per_stream + publishers x <pipeline cost>`, and a
configuration that presents only the first term as *the* memory setting
understates the node.

It stays a constant rather than becoming a knob because an operator has no
basis on which to choose a value: the figure is driven by track count and
group-of-pictures structure, which are properties of the publisher rather than
of the deployment. A knob whose correct value is unknowable invites tuning
that can only break discovery for multi-rendition contributors. What is owed
instead is the guarantee and a way to check it: the per-publisher ceiling
named in `config.md`, the reference stating that `memory_per_stream` covers
retained media only, and a metric for pipeline bytes in use — per publisher
and in aggregate — so the promise is verifiable on the box rather than only
asserted here. If a deployment later proves it needs the cap, the additive
move is a `[capacity] memory_per_publisher` sibling.

**Decided: no `memory_per_publisher` knob for now.** Ship the guarantee, the
documented figure, and the metric. The knob stays a named future move rather
than part of this adoption, because a value an operator cannot derive from
anything they observe is not a setting they can use, and the metric is what
would tell them they need one.

It appears commented in the reference so the memory model is visible where
operators read, and it is intended to land — **last**, after everything else
in this adoption. The ordering is not arbitrary: reporting the total is a sum
over figures the buffers already maintain, while enforcing one shared budget
means deciding what a stage does when another holds the bytes it wants. Fail
the session and a transient peak kills a healthy publisher; block and a memory
cap becomes a stall. That choice should be made against real numbers from the
metric rather than ahead of them.

Pre-roll dominates that total and must **not** be shrunk on the strength of
its size alone. Its horizon is applied per track, so a multi-rendition
publisher legitimately needs the headroom: pre-roll cannot lock until every
video track has shown a compatible keyframe cadence, and samples are charged
at retained cost — `size_of::<NormalizedSample>() + payload` — so many small
access units consume the budget far faster than their payloads suggest.

**A process-wide view of ingest rate.** The density limits are per session, so
a full node of publishers each sitting just under its own limit is unbounded
in aggregate. A metric at minimum.

**Drop `maximum_bytes_per_media_second`.** 100 Mb/s is inside what a 4K
multi-rendition contribution can reach, and a publisher throttled on a node
whose operator set no limit is the surprise this design exists to remove. It
is not load-bearing for memory: the batch and channel caps bound retained
bytes on their own, and this one bounds only sustained rate, which the link
already bounds. The packet and sample density caps stay — they catch
degenerate inputs, such as floods of empty access units, that byte caps cannot
see, and no legitimate encoder approaches them.

**Tier caps are bytes only.** A duration form was considered and dropped: at a variable
bitrate it has no fixed byte meaning, so it would be a second, weaker way of writing
`retain`. `retain` stays the time promise, the tiers stay the spend.

**A disk tier.** The store has no filesystem path anywhere today, so this is a
real subsystem: write path, crash recovery, orphan reaping, and a read path
that is slower than memory without stalling the live edge.

**Effective-depth reporting, per stream and per tier.** Configured retention is
a request; what a stream actually holds depends on its bitrate. Without this,
an administrator who asked for two hours and received seven minutes has no way
to find out.

## Delivery

**An operator-facing `hold_back`.** `DeliveryTimingPolicy::part_hold_back` is
a compiled three-times-part-target multiple today and nothing reaches it. It
becomes a `DurationRule` under `[hls]`, accepting the multiple and absolute
forms already used by the stall rules.

Two thresholds guard it, because the specification has two. Below **two** part
durations is refused: `EXT-X-PART-HOLD-BACK` MUST be at least twice the part
target, so a value the protocol forbids cannot be honoured approximately, and
advertising it anyway makes clients stall. Between two and three is
**warned**: three is a SHOULD, and a deployment on a controlled network may
legitimately want the lower latency. Refusing there would deny a legal choice,
and silently raising it would ignore an unambiguous instruction — so the value
is honoured and the operator is told what they gave up.

**The blocking-reload deadline stops being independent.** It is a separate
three-times multiple in the same policy, so an operator raising `hold_back`
past it would produce reload deadlines that expire on precisely the requests
the advertised hold-back invites. It must be derived as
`max(3 x target, hold_back + part)`. This relationship belongs in the
derivation itself rather than in a comment beside two constants that happen to
agree: written as two independent defaults, the next person to retune either
one has nothing telling them the pair is load-bearing.

**Delta playlists, and no `playlist` window.** `retain` is both what stays
fetchable and what the playlist advertises; the separate advertised window is
dropped. A player can only request what a playlist names, so a shorter
advertised window makes the remainder unreachable rather than cheaper.

The argument for splitting them was reload cost — a long window means
re-sending every segment line on each reload — and that is precisely what
Playlist Delta Updates remove. Once `CAN-SKIP-UNTIL` is advertised, a reload
costs what changed, so the motivation for a short advertised window over deep
retention disappears rather than being served by a second knob.

The skip boundary does not become a setting either. The specification fixes it
at no less than six target durations, and it bounds what one delta response may
omit, not what the playlist advertises — a client without a prior copy still
gets the full window. It is derived from `segment`.

Delta playlists stay withheld until they can ship whole: advertising a skip
boundary commits the origin to rendering skipped playlists, and `EXT-X-SKIP`
requires `EXT-X-VERSION` 9. Collapsing the two windows into `retain` is
independent of that work and can land first; what depends on deltas is whether
a *deep* `retain` is affordable to advertise, which is a performance property
rather than a correctness one.

The one thing that must not be lost is the grace period for a segment that has
just left the window while a client is still fetching it. That is seconds,
derived from cadence, and is the only sense in which anything outlives the
advertised window.

**Playback authorization.** Entirely absent today: the delivery surface has no
notion of a viewer identity, and authorization exists only for publishers. Adds
local token verification on the request path, which must stay cheap — it runs
on every playlist reload and every segment — plus key loading, optional key-set
refresh, and a claim naming the stream a token admits.

## Export

**`[record]`**: a filesystem sink with pattern expansion. No URL form.

**Atomic writes**: temporary name on the same filesystem, sync, rename, sync
the parent directory. The temporary path sharing the destination's filesystem
is a correctness requirement, not a preference.

**Initialization prepended to every export.** Both recorded files and hook
payloads. Delivery over HLS is unchanged and keeps using `EXT-X-MAP`. There is
precedent in the codebase: the subtitle muxer already concatenates its header
into segment bytes.

## Hooks

**Binary-mode rendering** in the dispatcher, selected per destination by
`payload`. Structured mode already exists.

**Four extension attributes**: rendition, segment, duration in milliseconds,
discontinuity.

**A `segment.ready` event type**, at segment cadence — substantially noisier
than the existing lifecycle events, and the first event whose body may be
megabytes. Payload-carrying deliveries move real bandwidth through a queue
sized for small JSON, so backpressure and drop behaviour need review.

## Configuration layer

**Named policies only; no inline predicates in a response.** A response may
carry `policy`, naming an entry under `[accept.policy]` that replaces
`[accept]` wholesale, and nothing else about media. This is close to what the
code already does: `BTreeMap<String, StreamPolicy>` resolved at startup, with
an unknown name failing closed. Three changes remain — policies move under
`[accept.policy]` and take the new predicate schema, a policy may widen as
well as narrow the compiled default, and the reserved `default` name goes away
in favour of `[accept]` itself being the unnamed default.

The module note in `admission/http` arguing that a response must never carry
policy is therefore *upheld* rather than reversed, but its reasoning is
replaced: it defends against an untrusted sidecar, where the actual argument
is legibility — the file stays the whole truth about what the node accepts,
reviewable and startup-validated, and the response only chooses among sets it
defines.

**Client certificates on outbound calls.** Built for `[auth.publish]`.

The listener's rotation machinery is reused rather than duplicated: the
resolver behind its `ArcSwap` now answers both `ResolvesServerCert` and
`ResolvesClientCert`, and `ClientIdentity` owns a watch of the same kind. A
client certificate expires exactly as a server one does, so giving the two
different reload stories would have meant one of them silently not reloading —
and a node that cannot renew without a restart drops publishers on rotation
day.

Resolution stays per handshake, so nothing rebuilds the client or rebinds a
socket. Loading is fatal at startup and non-fatal on reload, matching the
listener: a node told to present an identity it cannot read should not come up
pretending it will be admitted, but a bad rotation should not start failing
every call either.

A pinned `ca` replaces the platform store rather than adding to it, because a
deployment pinning its own authority wants that authority and not every public
one as well. Trust roots are deliberately *not* watched: a root is what a
rotation is validated against, and turns over on a far slower schedule than the
leaf it signs.

A destination configuring any of this gets its own connection pool. Pooling
across destinations would mean presenting one service's certificate to another,
so the cost of asking for mutual TLS is one pool per destination that asks.

**The same three fields on hook destinations.** A hook endpoint is as much an
operator-run service as the admission one, reached over the same networks, so
the shape is identical rather than merely similar.

What made this a dispatcher change rather than a configuration field is that
`cc-hooks` held *one* client for every destination. `HookConfig` now carries an
optional client of its own, and the dispatcher prefers it over the shared one.
Optional rather than required because the default is worth keeping: a node with
four plain endpoints reads the platform trust store once and shares one
connection cache, and only a destination that asks for an identity or a pinned
authority pays for a pool. That also keeps the property from admission — a
pooled connection reused across destinations would present one service's
certificate to another.

Loading is fatal at startup for hooks too, and errors name the hook, so an
operator running several knows which one to fix.

**An NTSC alias table in frame-rate parsing.** Four decimal spellings map to
their true rationals before any comparison. Parsing only — `FrameRate::exceeds`
is already exact.

**Environment values parse as TOML fragments.** The existing `TomlTable`
`value_parser` pattern generalised: structured predicates stay overridable and
the file and environment share one parser.

**A metrics listener of its own.** Metrics are served on the HTTP listener
today. Needs a third bound address, defaulting to loopback. Configuring it to
the HTTP address keeps the current shared-port behaviour, which means the
router must serve metrics on either listener depending on how they resolve.

**A derived handshake timeout.** One operator-facing RTMP timeout fans out to
an established-session limit and a shorter unauthenticated-handshake limit.
The per-phase overrides that exist today are removed.

**Enhanced RTMP validation is always strict.** `EnhancedValidationMode` and
its `passthrough` arm are removed rather than left unreachable. Malformed
Enhanced FLV framing or invalid capability fields refuse the publisher, on the
same footing as a failed handshake: a publisher that cannot describe its own
media correctly is not one this origin should package. Keeping opaque bytes
instead defers the failure to a layer with less context and turns a protocol
error into a packaging error. If real publishers turn out to violate the
specification in practice, the answer is to decide deliberately which
deviation to tolerate, not to leave a switch that disables the whole check.
This is separate from `[accept]`, which decides *which* codecs are admitted;
this decides whether the framing is well-formed at all.

**`[srt] timeout` becomes operator-facing and keeps its `latency` floor.**
`SrtConfig::peer_idle_timeout` exists and is already validated as strictly
greater than `latency`; the reference now exposes both, so the invariant moves
from an internal default pairing to a relationship between two configured
values and must be refused rather than silently adjusted.

**`floor.pace` must be refused at or above `ceiling.pace`.** A ceiling holding
a publisher at exactly the floor makes ordinary jitter fatal, and no value of
the pair is usable, so this is a refusal rather than a warning.

**Exact-rational frame rate comparison and unit aliases everywhere.** Rates
compare as rationals rather than floats, and the friendly sample-rate spelling
parses in every position including inside a range.

**Environment interpolation in string values.** `${VAR}` canonical, `$$` for a
literal dollar, undefined is an error, `${VAR:-fallback}` for a deliberate
default. String leaves only. The insertion point is clean: the file is already
parsed into a value tree before the settings layer sees it, so substitution
happens there and still composes with environment and command-line overrides.

Built. The braces are what let a value be composed from several variables and
sit against surrounding text, and they keep the sigil distinct from
`[record] pattern`, whose `{stream}` and `{time:...}` placeholders carry no
`$` and are expanded per segment by a different layer. One resolves once at
startup from the environment; the other resolves per file from media.

**An inline form for every secret**, alongside the existing `_file` forms.
Setting both is refused rather than ordered by precedence.

**A node name.** Nothing identifies the process today. It defaults to the
hostname and feeds metric labels, log fields, and the producer identity on hook
events, replacing the separately-configured CloudEvents source.

**One shutdown budget covering both drains.** A hook drain deadline exists
today; the viewer-facing drain is not configurable, so a restart either cuts
parked playlist reloads or outlives the orchestrator's grace period with no
lever either way.

**Hidden knobs leave `AppConfig` entirely.** Anything remaining there keeps a
working environment-variable name whether or not the file documents it, and an
undocumented variable becomes an interface as soon as someone finds it.

They stay visible as decisions in code: a named constant or a default
implementation at the layer that owns them, not a literal inlined at a call
site. The pending-admission budget in particular should derive from the
configured stream budget rather than remaining a fixed number.

**Drift protection.** Decoupling the file from library defaults removes the
current trick of reading defaults back out of the library. A test asserting
that each shipped file resolves to the intended runtime configuration has to
replace it, or the independence silently reintroduces drift.

## Packaging

The container image ships `rushls.reference.toml`, the hardened surface, so a
default container is not an open origin. `rushls.toml` is the local loopback
starter and must not become the image default.

## Deferred

Out of scope for this design, neither specified nor planned here:
per-hook rendition filtering, and transcoding with any rendition ladder.

Everything else in this ledger is specified in `config.md` and pending
implementation. In particular, playback authorization and environment
interpolation are designed, not deferred to a later design.
