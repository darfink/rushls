# RFC companion: what the configuration asks the application to become

> **Status: proposed.** A ledger, not a plan. Nothing here is implemented and
> no ordering is committed to.
>
> This exists because [config.md](config.md) deliberately designs the
> configuration **without regard for what the internals currently look like**.
> That is the point of the exercise: the operator surface is chosen first, and
> the application changes to serve it. This file is the honest accounting of
> what that costs, so the cost is visible while the design is still cheap to
> change.
>
> Every entry is a place where the proposed configuration describes behaviour
> the application does not have yet. None of them are reasons to change the
> configuration — they are the work the configuration implies.

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

## Health

**`stall` stays idle-based.** No work needed, recorded because it was nearly
specified as behind-ness against wall clock, which the evaluator does not
measure and which would disconnect a slightly slow encoder on a timer. The
existing source and media idle checks collapse into one operator-facing
duration.

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

**Delta playlists.** Not required for long retention with a short advertised
window, which is the useful configuration today. Required before a long
*advertised* window is practical, since without them every client re-fetches
the full segment list on each reload. Currently withheld deliberately:
advertising a skip boundary commits the origin to rendering skipped playlists,
and the two must ship together.

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

**A metrics listener of its own.** Metrics are served on the HTTP listener
today. Needs a third bound address, defaulting to loopback. Configuring it to
the HTTP address keeps the current shared-port behaviour, which means the
router must serve metrics on either listener depending on how they resolve.

**A derived handshake timeout.** One operator-facing RTMP timeout fans out to
an established-session limit and a shorter unauthenticated-handshake limit.
The per-phase overrides that exist today are removed.

**Exact-rational frame rate comparison and unit aliases everywhere.** Rates
compare as rationals rather than floats, and the friendly sample-rate spelling
parses in every position including inside a range.

**Environment interpolation in string values.** `${VAR}` canonical, `$$` for a
literal dollar, undefined is an error, `${VAR:-fallback}` for a deliberate
default. String leaves only. The insertion point is clean: the file is already
parsed into a value tree before the settings layer sees it, so substitution
happens there and still composes with environment and command-line overrides.

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
