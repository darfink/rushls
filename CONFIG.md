# Designing the Rushls configuration for humans

> **Status: open discussion / living document.** This captures where our
> thinking on the configuration surface has landed and why. Nothing here is
> built yet. It is meant to be argued with and revised, then to become the spec
> the `AppConfig` / `resolve()` rewrite implements.

## The question

How do we make the Rushls configuration as user-friendly as possible given what
the application actually is — an HLS (and eventually multi-protocol) ingest and
delivery server — without losing the flexibility a power user needs?

Two goals pull against each other and we want both:

- **Intuitive.** A newcomer should be able to configure what they want without
  reading an essay per field, and without understanding how the pipeline works
  internally.
- **Flexible, not pedantic.** Power users keep their niche knobs, but those
  knobs must not sit at the same visual altitude as "what port do I listen on,"
  and the config must not nag the operator with refusals over relationships they
  didn't know existed.

## What's wrong with the current config

The current `rushls.toml` is well-built and the prose is genuinely good — but it
is written as *a complete reference with every default spelled out*, and it
doubles as the file a new user opens first. The result is ~45 knobs and ~16 KB
of commentary for a server whose essential configuration is really about six
lines. Three distinct problems fall out of that.

### 1. The surface is too flat and too verbose

Power-user internals sit next to essential settings. The file's own header says
it "contains every supported setting," which guarantees verbosity by design.
Starter config and exhaustive reference are forced to be the same document, so
neither is good at its job.

### 2. There is no low-effort path to "permissive"

A local tester who thinks *"just remove the limits, I only want to test"* has no
single lever to pull. They'd have to find and individually neutralize settings
scattered across five sections:

- the whole `[auth.policies.default]` block — track counts, resolution, frame
  rate, three codec allowlists, takeovers;
- `faster_than_realtime` (a file push at full speed is *paced or rejected* by
  default — the single most likely thing to confuse a local tester feeding it an
  MP4);
- `[server]` concurrency + pending caps;
- `[storage]` buffer/stream/retention limits;
- `[ingest.rtmp]` timeouts, `[ingest.srt]` idle timeout, `[ingest.health]`
  stall detection.

Worse, several of these can't even be set to "unlimited": there is no
`max_resolution = "off"`, no `max_buffer_per_stream = "unlimited"`. The config
has **no concept of a stance**, so "I don't care, let me test" is not
expressible.

### 3. Internal machinery leaks into the operator surface

The module docstring already claims the right principle — *"describes
administrator intent, not the shape of the internal pipeline"* — and the
architecture to honor it exists (`AppConfig` → `resolve()` → `NodeConfig`). But
for the leaky settings `resolve()` is doing a near-*identity* mapping: the
config field is named and shaped after its internal target, so the translation
seam exists but buys nothing.

Worst offenders, worst first:

- **Pacing is the internal enum, verbatim.** `faster_than_realtime` +
  `maximum_lead` + `maximum_timestamp_jump` is a direct transcription of
  `IngestTimingPolicy::{PaceToRealtime { .. }, RequireRealtime { .. }}`. The
  three-knob shape, the field names, and even the "jump is refused under
  `reject`" rule exist only because that's how the enum is carved.
- **The health block is pipeline stages.** `source_stall` vs `media_stall` is
  the internal "input delivers nothing" vs "normalizer produces nothing while
  input arrives" boundary — meaningless to an operator, who has one concept:
  *the stream is dead*. `health_interval` is the supervisor's sampling period;
  `publication_stall_multiplier` is `stalled_publication_multiplier`.
- **Auth mirrors trait dispatch.** Three mutually-exclusive tables
  (`[auth.open]` / `[auth.static]` / `[auth.http]`) with "can't combine these"
  refusals are `Arc<dyn Authenticator>` implementations projected outward.
- **Hooks leak the dispatcher.** `queue_capacity`, `maximum_in_flight`,
  `maximum_attempts`, `maximum_response_bytes` are the delivery queue's
  internals.
- **`maximum_pending_publishers_per_listener`** is the most naked one — its
  own doc explains the field by describing the internal concurrency math.

The proof this is inconsistency and not taste: we **already did it right** once.
`CodecValue` is deliberately a narrower, config-only type than the internal
`Codec` (no `MovText`, no `Unknown`), with `From<CodecValue> for Codec` doing
the translation. The health, pacing, and hooks sections should look like that
and don't.

**The test to apply to every field:** could you rename or restructure the
internal type without touching the config? For `CodecValue` you can gut `Codec`
freely. That's the target state for every section.

## Design principles

1. **The default/starter config is the local-test config.** Limits are what you
   *add* when moving toward untrusted networks, not what you *remove* to get
   started. The current file hands you the hardened stance and makes you
   dismantle it; that's backwards.
2. **Design the operator vocabulary first, on its own terms** — then convert to
   internal equivalents in `resolve()`. Ignore the internal machinery while
   choosing names and shapes.
3. **Give `resolve()` real translation work.** One config field may fan out to
   several `NodeConfig` fields. That fan-out is the anti-corruption layer
   earning its keep.
4. **Expose internal granularity only when an operator has a genuine, distinct
   reason to want it** — and when you do, put it in an advanced tier, not at the
   altitude of `listen`.
5. **`"off"` / `"unlimited"` is a first-class value everywhere a limit exists.**
   The pattern already exists for timeouts; extend it to resolution, buffers,
   track counts, stall detection.

## The design

### Two shipped files instead of a `mode` field

We first considered a top-level `mode = "local" | "production"`. We're dropping
that in favor of **two files checked into the repo**:

- `rushls.toml` — minimal dev/local starter. Frictionless: push a file, watch it
  play. Ideally near-empty plus comments.
- `rushls.prod.toml` — the hardening template *and* the exhaustive reference.

This is better than `mode` because the file *is* the stance: there's no
field-by-field precedence puzzle to define, and starter-vs-reference stop
fighting to be one document. Operators select a starting point by copying a
file, which is a more obvious gesture than discovering a magic keyword.

**Open decision — compiled-in defaults when no file is given.** With `mode`
gone, the stance lives in the compiled defaults. Recommendation: **permissive
compiled defaults, plus a loud startup warning** whenever the running config is
simultaneously exposed and unlimited (no auth + limits off + a non-loopback
listen). This keeps local frictionless without the silent-exposure footgun, and
reuses the existing `warnings: Vec<String>` channel for "legal but probably not
what you meant." The alternative — hardened defaults — is safe if the file is
deleted but pushes verbosity back into the local path. *This is the last item to
settle before the spec is buildable.*

Precedence chain stays: **compiled defaults < selected TOML (interpolated) <
`RUSHLS_*` env < CLI.**

### Environment interpolation inside config values

Override-envs (`RUSHLS_*`) can *replace* a setting but can't let you reuse an
env some orchestrator already injected under its own name, or compose one:

```toml
url   = "http://${AUTH_HOST}:${PORT}/admit"
token = "${EXISTING_SECRET}"
```

Insertion point is clean: `load_from` already parses the file into a
`toml::Value` before handing it to `conf`'s `.doc(...)`. Interpolate string
leaves there, before `conf` sees it, so interpolation composes with — and is
then overridable by — the `RUSHLS_*` envs and CLI.

Grammar rules to pin down (interpolation is easy to ship with sharp edges):

- **`${VAR}` is canonical** (unambiguous boundaries for composition). Bare
  `$VAR` optional; docs show `${...}`.
- **Escaping:** `$$` → literal `$`. SRT passphrases and publishing keys can
  legitimately contain `$`; silent interpolation would corrupt them.
- **Interpolate string leaves only** — never table names or keys.
- **Undefined variable is an error, not empty** (fail-closed: substituting `""`
  into a `token` could disable auth). Provide `${VAR:-fallback}` so "empty" is
  opt-in and visible.
- **Still steer new secrets toward `_file`.** Interpolation is a convenience for
  *existing* envs; for new secrets, `token_file` / `key_file` on a restricted
  mount remains the better story (envs leak into `/proc`, crash dumps, `ps`).

### Section layout — flat, HLS-namespaced for the future

We keep sections flat and top-level (matching `[ingest]`, `[storage]`, `[http]`)
rather than introducing a neutral `[delivery]` parent for a single child today.

To keep flat `[hls]` future-proof for a later `[dash]` sibling, the rule is:
**anything HLS and DASH would both need does not go under `[hls]`.** Shared,
protocol-neutral settings live where they conceptually belong instead:

- `segment_duration` — a property of how media is *cut* (shared CMAF
  segmentation), so it's a top-level/global output setting, not HLS-only.
- `base_url` — how *this node is addressed* by clients/CDN, so it's an `[http]`
  concern (the public face of the HTTP listener).

`[hls]` then holds only genuinely HLS-manifest-specific fields, so adding
`[dash]` later is purely additive and nothing has to move.

```toml
[hls]
low_latency     = true     # LL-HLS parts
playlist_window = "6x"

# added later, a sibling — no shared settings trapped inside either:
# [dash]
# ...
```

### The five operator questions

Everything an operator wants falls into five buckets, and the config should have
roughly five areas in this order. Note what is *not* on the list — sampling
intervals, pending-handshake counts, pace-vs-reject — those are answers to
internal questions and drop below the fold.

1. What stance am I running in? → choice of shipped file
2. How do streams come in? → `[ingest.*]`
3. Who may publish, and what will I accept? → `[auth]` + `[accept]`
4. How does playback behave? → global output settings + `[hls]`
5. Where do I keep things / who gets told? → `[storage]`, `[webhooks.*]`,
   `[metrics]`

### Local starter (`rushls.toml`)

A complete, working config for "push a file and watch it play" should be
essentially empty — the compiled permissive defaults do the rest:

```toml
# Local development starter. Boots RTMP :1935, SRT :9000, HTTP :8080,
# accepts any codec/resolution, plays file pushes as live, no auth.
# Copy rushls.prod.toml as a starting point for a hardened deployment.
```

### Production template (`rushls.prod.toml`)

The full hardened surface — and every line reads as intent:

```toml
[ingest.rtmp]
listen = "0.0.0.0:1935"

[ingest.srt]
listen     = "[::]:9000"
latency    = "120ms"
passphrase_file = "/run/secrets/rushls-srt-passphrase"

[auth]
# open | keys | service
mode = "keys"

[auth.keys.front-camera]
stream   = "live/camera"
key_file = "/run/secrets/rushls-publish-key"

[accept]
file_pushes    = false        # only genuine live streams
codecs         = "common"     # "common" | "any" | ["h264","aac", ...]
max_resolution = "4k"         # "1080p" | "4k" | "8k" | "1920x1080" | "off"
max_frame_rate = "60"         # or "off"

# shared / global output settings (not HLS-specific)
segment_duration = "6s"

[hls]
low_latency     = true
playlist_window = "6x"

[storage]
reconnect_window      = "30s"
max_buffer_per_stream = "512MiB"   # or "off"

[http]
listen   = "0.0.0.0:8080"
base_url  = "https://cdn.example.com/live"   # empty = relative URLs

[http.cors]
origins = ["https://player.example.com"]

[http.tls]
certificate = "/etc/rushls/tls/fullchain.pem"
key         = "/etc/rushls/tls/private-key.pem"

[metrics]
enabled    = true
token_file = "/run/secrets/rushls-metrics-token"

[webhooks.automation]
url        = "http://automation:9000/rushls/events"
events     = ["session.started", "stream.available", "stream.unavailable", "session.ended"]
token_file = "/run/secrets/rushls-hook-token"
```

### Advanced tier

Niche power-user knobs still exist, but under explicitly-labeled tables so their
optionality is visible. The word `advanced` in the path does the tiering the
current flat file can't:

```toml
[ingest.rtmp.advanced]
handshake_timeout = "3s"
session_timeout   = "30s"

[limits.advanced]
concurrent_publishers = 256
pending_per_listener  = 64

[http.tls.advanced]
handshake_timeout  = "5s"
pending_handshakes = 256
```

## The key renames / restructures

- **`[accept]` replaces the pacing enum and the codec/limit sprawl.**
  `file_pushes = true|false` is the honest form of `faster_than_realtime` — a
  boolean intent, not a `pace`/`reject` mechanism with two coupled thresholds.
  `codecs = "common"` and `max_resolution = "4k"` use friendly aliases with
  `"off"` first-class.
- **`[auth] mode = open|keys|service`** replaces three mutually-exclusive tables
  and their "can't combine" refusals. One decision, then a sub-table only for
  the mode chosen. The polymorphism stays internal.
- **`[webhooks.NAME]` shrinks to `url` / `events` / `token`.** Queue depth,
  in-flight, attempts, response ceiling move to defaults or advanced.
- **Stall detection disappears from the surface.** At most one
  `stall_timeout = "1x" | "off"`; the four-way split stays internal.
- **Per-phase timeouts, TLS handshake tuning, pending caps → advanced.**

## Mapping the surface onto `NodeConfig`

This is where `resolve()` starts earning its keep — one operator field driving
several internal ones. The interesting rows:

| Operator field | Fans out to (internal) |
|---|---|
| shipped `rushls.toml` (local) | permissive policy + `file_pushes=true` + stall off + relaxed timeouts + `auth.mode=open` |
| `accept.file_pushes = true` | `IngestTimingPolicy::PaceToRealtime { maximum_lead: <derived>, maximum_timestamp_jump: <derived> }` |
| `accept.file_pushes = false` | `IngestTimingPolicy::RequireRealtime { maximum_lead }` |
| `accept.stall_timeout = "1x"` | `health.source_stall_timeout` **and** `health.media_stall_timeout`; `health_interval` + `stalled_publication_multiplier` derived |
| `accept.codecs = "common"` | `accepted_{video,audio,subtitle}_codecs` (three lists) |
| `accept.max_resolution = "4k"` | `maximum_video_width` + `maximum_video_height` |
| `accept.max_resolution = "off"` | both set to their max sentinel |
| `hls.low_latency = true` | `SegmentationPolicy::latency_first` + a default `part_duration` |
| `segment_duration` | shared CMAF segmentation |
| `storage.reconnect_window` | `store.idle_retention` |
| `storage.max_buffer_per_stream = "off"` | `retention.maximum_payload_bytes = usize::MAX` |
| `auth.mode = "open"` | `OpenStreamAuthenticator(<permissive or named policy>)` |

The pacing row proves the coupling is gone: `faster_than_realtime` /
`maximum_lead` / `maximum_timestamp_jump` was a verbatim transcription of the
enum. Here one boolean picks the variant and the thresholds are derived — you
can recarve `IngestTimingPolicy` freely and the config doesn't move. That's the
`CodecValue`-vs-`Codec` discipline applied to the sections that lacked it.

## Open decisions

1. **Compiled-default stance (permissive vs hardened).** Recommendation:
   permissive + loud warning on exposed-and-unlimited. *Last blocker before the
   spec is buildable.*
2. **Drift guard.** Decoupling means config defaults and library defaults become
   two truths. Today the code prevents drift by reading values back out of the
   library (e.g. `default_maximum_timestamp_jump()` reads
   `StreamPolicy::permissive()`). Decoupling removes that trick, so we need a
   replacement: a test asserting each shipped file / default resolves to the
   intended `NodeConfig`. Cheap, but it must exist or independence silently
   reintroduces drift.
3. **Interpolation grammar edges.** Confirm `${VAR}` canonical, `$$` escape,
   undefined-is-error with `${VAR:-fallback}` opt-out, string-leaves-only.

## Guardrails, not nagging

The cross-field validation is mostly good, but a few rules cross into nagging:
refusing `maximum_timestamp_jump` under `reject` (the config's own TODO
questions this), refusing a handshake timeout longer than session timeout, an
SRT idle below latency, etc. Individually defensible; collectively they bounce
an operator tuning one number over a relationship they didn't know existed. For
the "probably a mistake but technically survivable" cases, prefer **warning**
over **rejecting** — matching the pattern already used for disabled timeouts and
tight pacing. Keep hard refusals for the genuinely unbootable.

## Next step

Turn this into the implementation spec: finalize the section layout and the two
shipped files, write the interpolation grammar precisely, complete the
field-by-field `→ NodeConfig` mapping, and settle open decision #1 in writing.
That spec is what the `AppConfig` / `resolve()` rewrite implements.

