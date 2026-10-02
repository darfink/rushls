# Configuration

Rushls runs without a configuration file: the compiled defaults are a working local origin.
A file is where you add listeners, limits, authorization, and storage.

```sh
rushls --print-config-example > rushls.toml   # the annotated example, matching this binary
rushls --config rushls.toml --check           # validate, print listeners and worst-case memory
```

The example lists every TOML setting, grouped by what it controls. Its only active settings bind the listeners to loopback.
Every setting, with its default, environment variable, and flag, is in the [configuration reference](configuration-reference.md).
This page explains how the settings behave.

## Files and precedence

Rushls loads the first file it finds:

1. `--config PATH` (fails if the file is missing)
2. `RUSHLS_CONFIG` (same)
3. `./rushls.toml` in the working directory
4. `rushls.toml` in your user configuration directory:

   | Platform | Directory |
   | --- | --- |
   | Linux | `$XDG_CONFIG_HOME/rushls`, which defaults to `~/.config/rushls` |
   | macOS | `~/Library/Application Support/rushls` |
   | Windows | `%LOCALAPPDATA%\rushls\config`, then `%APPDATA%\rushls\config` |

A system service usually sets `RUSHLS_CONFIG`, for example to `/etc/rushls/rushls.toml` as the container image does.

It logs which file it used, or that it used the compiled defaults.

Values are resolved in this order, later wins: compiled defaults, the file, environment variables, command-line flags.
Environment variables are named `RUSHLS_` plus the setting path, such as `RUSHLS_HLS_SEGMENT` for `hls.segment`.

```sh
RUSHLS_HLS_SEGMENT=6s
RUSHLS_PUBLISH_STRICT=true
RUSHLS_PUBLISH_RATE='{ max = "1x", burst = "10s" }'
RUSHLS_MEMORY_TOTAL=16GiB
```

Structured values (`publish.rate`, `publish.video`, `publish.audio`, `publish.subtitles`, `hls.segment`, `hls.part`)
use their TOML spelling in the environment and on the command line, and replace the whole value.
Named profiles, hooks, recording, and playback claims can only be set in the file. `rushls --help` lists every flag.

Unknown keys stop startup with the path of the offending key. An unknown `RUSHLS_` environment variable logs a warning and is ignored.

The file is read once, at startup. Rotated TLS certificates and JWKS keys take effect without a restart; every other change needs one.
`--check` exits with status 2 on an invalid configuration.

## Writing values

| Kind | Examples |
| --- | --- |
| Duration | `"500ms"`, `"10s"`, `"2h"` |
| Size | `"64KiB"`, `"128MiB"`, `"8GiB"` |
| Multiple of a related target | `"3x"`, `"1.5x"`; for example `hls.window = "6x"` is six segment targets |

### Turning things off

Omitting a table turns its feature off. `"off"` lifts a single limit, and only where a setting accepts it.

| Omitted | Result |
| --- | --- |
| `[publish.auth]` | Anyone may publish |
| `[playback.auth]` | Anyone with the URL may watch |
| `[metrics]` | No metrics are served |
| `[record]` | Nothing is recorded |
| `[https]` | No HTTPS listener |
| `[tls]` | No certificate, so no HTTPS, RTMPS, or MoQ |
| `[disk]` | Streams stay in memory |
| `publish.rate` | No pace limits |

These accept `"off"`: `ingest.rtmps.listen`, `ingest.moq.listen`, `http.listen` (but not together with no HTTPS),
`ingest.stall_timeout` and `ingest.idle_timeout` (`"none"` also works), and `http.cors.origins`.
`memory.total` and `memory.per_publisher` accept `"unlimited"`.
Other durations, counts, and sizes must be positive, and `[publish]` predicates are removed by omitting them.

### Secrets

Every credential is a single value, and its shape says where it comes from:

```toml
[metrics]
listen = "127.0.0.1:9090"
token = { file = "/run/secrets/metrics-token" }   # a mounted file (preferred)
# token = "${METRICS_TOKEN}"                      # an environment variable
# token = "literal"                               # inline
```

A mounted file keeps the secret out of process listings and out of a committed configuration file.
Trailing CR/LF characters are removed from files; spaces are kept.

`${VAR}` works in any string value. An undefined variable is an error rather than an empty string;
write `${VAR:-fallback}` for a default, and `$$` for a literal `$`.
In the environment, a value shaped like `{ file = "/path" }` names a file: `RUSHLS_METRICS_TOKEN='{ file = "/run/secrets/metrics" }'`.

On the command line, a credential flag always takes a file path, such as `--metrics-token /run/secrets/metrics`,
because any user on the machine can read process arguments.
The credentials with flags are `ingest.srt.passphrase`, `publish.auth.token`, `playback.auth.secret`, and `metrics.token`.
Hook `token` and `signing_secret` take the same three forms in TOML, so `token = "${HOOK_TOKEN}"` reads an environment variable.
They have no `RUSHLS_` variable or flag of their own, because the hook's name is yours to choose.

## Listeners

| Table | Default | Notes |
| --- | --- | --- |
| `[ingest.rtmp]` | `0.0.0.0:1935` | |
| `[ingest.rtmps]` | off | Needs `[tls]`; accepts TLS 1.2 and 1.3 |
| `[ingest.srt]` | `0.0.0.0:9000` | IPv4 only |
| `[ingest.moq]` | off | UDP; needs `[tls]`; see [MoQ](moq.md) |
| `[http]` | `0.0.0.0:8080` | Playback and health probes |
| `[https]` | off unless the table is present | Needs `[tls]` |
| `[metrics]` | off unless the table is present | See [Metrics](metrics.md) |

RTMP and HTTP also accept `[::]` addresses.

### Certificates

One `[tls]` table serves every TLS listener: HTTPS, RTMPS, and MoQ.
Rushls reloads the files when they change; an invalid replacement leaves the previous certificate in use.

```toml
[ingest.rtmps]
listen = "[::]:1936"

[https]
listen = "[::]:8443"
min_version = "1.2"         # default: TLS 1.3 only

[tls]
cert = "/etc/rushls/tls/fullchain.pem"
key  = "/etc/rushls/tls/private-key.pem"
```

`https.min_version` is `"1.3"` or `"1.2"`; TLS 1.3 is always accepted. It does not affect RTMPS, which always accepts both, or MoQ, which always uses TLS 1.3.
Configuring `[https]`, RTMPS, or MoQ without `[tls]` stops startup; a `[tls]` that nothing uses logs a warning.

### Behind a proxy

A TLS terminator or TCP load balancer in front of RTMP hides the publisher's address.
If the proxy sends the PROXY protocol (v1 or v2), set `proxy_protocol = true` on `[ingest.rtmp]`, or on `[ingest.rtmps]` for a balancer that passes TLS through.
Every connection must then start with the header, or it is refused, so the listener must be reachable only through the proxy.
SRT and MoQ run over UDP, where the source address is normally preserved.

`http.public_url` sets an absolute prefix for URLs inside playlists. Empty, the default, makes them relative, which is right behind a proxy or CDN.
See [Deployment](deployment.md#reverse-proxies-and-cdns) for proxy and CDN setup.

### HTTP limits and CORS

`http.max_connections` and `http.max_requests` (both 4096) are shared by the HTTP, HTTPS, and metrics listeners.
Extra connections are closed; extra requests get `503` with `Retry-After: 1`.
A request holds its slot while it waits for media, as a blocking playlist reload does.

`[http.cors]` controls cross-origin playback. `origins` defaults to `*`; list origins to restrict it, or set `"off"`.
`expose_headers` lists the response headers a cross-origin player can read:

```toml
[http.cors]
origins = ["https://player.example.com", "https://*.example.com"]
expose_headers = ["content-length", "content-range", "date"]   # the default
```

## Publishers

`[publish]` sets the terms every publisher gets: which media it may send, how fast, and what happens on a second publisher for the same stream.

### Media rules

`video`, `audio`, and `subtitles` restrict what a publisher may send. Omit a field to allow anything Rushls can package.

```toml
[publish]
video = { codecs = ["h264", "hevc"], resolution = { max = "1080p" }, frame_rate = { max = 60 }, tracks = { max = 4 } }
audio = { codecs = ["aac", "opus"], sample_rate = [44100, 48000], channels = { max = 2 } }
subtitles = { tracks = { max = 2 } }
```

Each field takes one of three forms:

| You write | Means |
| --- | --- |
| `frame_rate = 60` | exactly 60 |
| `frame_rate = [24, 25, 30]` | one of these |
| `frame_rate = { min = 24, max = 60 }` | a range; both ends inclusive and optional |

A single value is exact, not a maximum: `tracks = 1` means exactly one track, and "at most one" is `{ max = 1 }`.

- **Frame rates** compare exactly. `29.97` and `"30000/1001"` are the same rate; so are `23.976`, `59.94`, and `119.88` with their `/1001` forms.
  `frame_rate = 30` does not admit 29.97; use a range to accept both.
- **Sample rates** are in Hz: `48000`, or `"48kHz"`, in any position.
- **Resolution** is a bounding box. `{ max = "4k" }` means the picture fits inside 3840×2160, either way round, so portrait video fits too.
  `{ max = { width = 3840, height = 2160 } }` is the explicit form.
- **Codecs**: omitting `codecs` admits every codec Rushls can package, including ones added in later releases.

### Profiles

Named profiles let the [admission service](admission-and-hooks.md) give different publishers different terms:

```toml
[publish.profile.premium]
video = { resolution = { max = "4k" }, frame_rate = { max = 60 } }
audio = { channels = { max = 6 } }
```

An admission response `{"profile": "premium"}` selects it; without one, `[publish]` applies.
A profile replaces `[publish]` entirely. Settings it leaves out take the compiled defaults, not the values in `[publish]`.
A profile can therefore allow more than the default as well as less.
Profiles use the same keys as `[publish]`: `strict`, `takeover`, `rate`, `video`, `audio`, and `subtitles`.

### Strict input

`publish.strict` (default `true`) decides whether a hole in the input ends the publication or is served as a gap.
See [Input handling](input-handling.md).

### Pace

`publish.rate` bounds how fast media may arrive, in multiples of real time:

```toml
[publish]
rate = { max = "1x", burst = "10s", min = "0.5x", window = "30s" }
```

- **`max`** throttles. A publisher sending faster waits on its connection, so a file pushed at 100× plays as live.
  `burst` lets media run up to that far ahead of the clock, for example to catch up after a stall.
  Without `max`, the default, media is packaged as fast as it arrives, and a file pushed faster than real time plays like fast-forward.
- **`min`** disconnects. A publisher whose media advances slower than `min` on average over any `window` is dropped.
  The first window is a grace period. Without `min`, nothing is dropped for being slow, but a publisher below 90% of real time is logged, and logged again when it recovers past 95%.

`min` needs `window`, and `burst` needs `max`.

### Timeouts

```toml
[ingest]
idle_timeout  = "10s"   # connection carries no bytes at all: closed
stall_timeout = "12s"   # connected, but no usable media: dropped
```

`idle_timeout` applies to every protocol once a connection is established. The handshake has a shorter, fixed deadline.
For SRT, `idle_timeout` must be longer than `ingest.srt.latency`.

`stall_timeout` fires when packets keep arriving but none of them become media, for example keepalives only.

### Takeover

`publish.takeover` decides what happens when a second publisher connects to a stream that already has one.
The default, `false`, refuses the newcomer. `true` closes the current publisher and lets the newcomer continue the stream, with a discontinuity at the switch.

The default protects a live stream from anyone else holding its credential, but it has a cost:
a publisher whose connection drops silently keeps the stream until `stall_timeout` fires, so an encoder that reconnects sooner is refused.
With the default `stall_timeout` of 12 seconds, that can be 12 seconds of dead air after a network drop.

## HLS

### Segments and parts

```toml
[hls]
segment = "6s"   # or { target = "6s", max = "2x", tolerance = "0s" }
part    = "1s"   # or { target = "1s", max = "2x" }
```

These are the defaults. At startup, Rushls picks the segment length from the keyframes it sees:
the latest keyframe boundary common to every video track at or before `target`, or, if none fits, the first one up to `max`.
`max` is a duration or a multiple of `target`. `[hls.segment]` and `[hls.part]` tables work as well.

Parts divide segments for Low-Latency HLS. A regular part is between 85% and 100% of the part target;
a part that starts at a keyframe, or ends a segment, can be shorter.

### Irregular keyframe intervals

Once chosen, the segment schedule is fixed, and every later segment must end on a keyframe within `tolerance` of its planned time.
An encoder with an uneven keyframe interval (variable frame rate sources, scene-change keyframes, some OBS setups)
misses the window and ends the publication. The error says how late the keyframe was and which tolerance would have covered it:

```text
track/0: no keyframe for segment boundary at 16s (>= 4.12s late, tolerance 0s); set segment.tolerance >= 5s or fix the encoder keyframe interval
```

Fix the encoder first if you can: a fixed interval (`-g`, `-keyint_min`, and `-sc_threshold 0` in FFmpeg, or a keyframe interval in seconds in OBS)
keeps the tolerance at zero. Otherwise raise `tolerance`, and `max` if the segment no longer fits:

```toml
[hls]
segment = { target = "6s", max = "2x", tolerance = "2s" }
```

The cost applies to the whole publication. The tolerance raises `EXT-X-TARGETDURATION` by twice its value (6 s becomes 10 s here),
and players without Low-Latency HLS start three target durations behind the live edge, so their latency grows by six times the tolerance.
Low-Latency players follow parts and are unaffected. A tolerance absorbs keyframes that wobble around a steady interval;
an encoder that settles into a different interval mid-stream still fails, because every boundary is measured from the original schedule.

### DVR window

`hls.window` is how much media a playlist offers and how long it stays available, both during the publication and after the publisher leaves.
It is a duration or a multiple of the segment target (default `"6x"`), and must cover at least three target durations.

The window is a promise; memory and disk are what pay for it. If a stream's bitrate needs more than its storage allows,
the oldest media goes first and the stream offers less than the window. The retention depth is reported per stream in the metrics.
See [storage](#storage).

### Latency

`hls.hold_back` is how far behind the live edge Low-Latency players are told to start, and so the lower bound on their latency.
It is a multiple of the part target (default `"3x"`) or a duration.
Below two parts is refused, because HLS requires at least two. Between two and three parts is allowed with a warning;
players on lossy networks may stall there.

### Scrubbing

`hls.scrubbing` (default `true`) publishes an I-frame playlist per video rendition, which players use for seek-bar previews.
It points at the keyframes inside the regular segments, so it costs no extra storage.
With a 2-second keyframe interval there is one preview every 2 seconds; a 1-second interval gives denser previews.

## Capacity and memory

```toml
[limits]
publishers = 256              # concurrent publishers
streams    = 1024             # stored streams, live or ended but still within hls.window
publishers_per_address = 16   # optional; no per-client limit by default
```

`publishers` counts ingest connections. `streams` counts stored presentations, including ended ones still inside their window,
so a long window needs more streams than publishers. When `streams` is full, a new stream is refused rather than evicting a retained one.

`publishers_per_address` stops one client from occupying every publisher slot, for example with connections that stall during the handshake.
It counts pending and admitted connections across RTMP, SRT, and MoQ, and counts IPv6 clients per `/64`.
Behind an RTMP proxy it needs `proxy_protocol`, or every publisher shares the proxy's address.
It is off by default because a gateway or transcoder may legitimately publish many streams from one address.

There is no viewer limit; put a CDN in front of the origin to scale viewers.

### Storage

```toml
[memory]
per_stream = "512MiB"

[disk]
per_stream = "8GiB"
dir        = "/var/lib/rushls/dvr"
```

New media is stored in memory. When a stream's memory is full, its oldest media moves to disk; when both are full, the oldest media is dropped.
Media older than `hls.window` is dropped either way.

`memory.per_stream` also holds cached playlists. With `[disk]` configured, one eighth of it is kept for playlists,
and media starts moving to disk above 81.25% of the budget.

`disk.dir` is optional and defaults to the platform cache directory (`~/.cache/rushls/dvr` on Linux).
Setting `dir` without `disk.per_stream` is an error. The directory is locked, so two Rushls processes cannot share one.
Disk storage does not survive a restart: a new process starts empty and removes the old files.

Both budgets limit stored media. They do not cap the process's total memory.

### Publisher memory

```toml
[memory]
per_publisher = "128MiB"   # minimum 64MiB, or "unlimited"
```

Each publisher's media in flight (received packets, the startup buffer, and packaging) shares one budget.
If the budget runs out, the publication ends with an error naming the stage that ran out.
While the startup buffer is holding media, the error also says how many samples it held, which is the usual cause.

A useful starting size follows from the total bitrate and how long Rushls buffers before choosing segment boundaries.
For example, 40 Mb/s held for eight seconds is about 38 MiB of media. Longer keyframe intervals mean longer buffering.
Watch the peak-usage metric to size it for your publishers.

`"unlimited"` removes the ceiling, for trusted publishers only. Memory is still measured and reported.

### Node total

```toml
[memory]
total = "16GiB"   # default "unlimited"
```

With a total, each active publisher reserves its `per_publisher` budget and each stored stream its `per_stream` budget,
and a new publisher whose reservation would exceed the total is refused as unavailable.
Reservations are returned when the publisher ends and when the stream leaves storage.
Because it reserves budgets rather than measuring use, a node can refuse work while actual use is well below the total.

`rushls --check` prints the worst case: `publishers × per_publisher + streams × per_stream`.
Neither budget counts everything the process uses, so leave headroom for the operating system, buffers, and allocator overhead.

## Recording

`[record]` writes each completed segment to a file. It does not record parts of unfinished segments, and recordings are not served or expired by Rushls.

```toml
[record]
dir  = "/var/lib/rushls/recordings"
path = "{stream}/{publication}/{time:%Y/%m/%d}/{rendition}_{segment}.mp4"
```

| Placeholder | Value |
| --- | --- |
| `{stream}` | Stream ID, with `/` preserved |
| `{publication}` | A UUID unique to this publication, across reconnects and restarts |
| `{time:...}` | Segment completion time in UTC, in `strftime` format |
| `{rendition}` | Rendition key, with `/` preserved |
| `{segment}` | Segment number within the publication, from zero |

Each MP4 file starts with its initialization data, so it can be decoded on its own. Subtitle files use `.vtt` instead of the configured extension.
Each rendition is a separate file; recording does not combine audio and video.

A file appears under its final name only once it is complete; ignore files starting with `.rushls-`.
An existing file is never overwritten. `dir` must be a local path; object-storage mounts often lack the filesystem operations recording needs.
Use a separate process to upload and expire recordings.

`queue_size` (128 segments) and `max_pending` (256 MiB) bound the recording backlog.
When either is full, the recorder drops the whole affected segment and reports a recording failure; live playback is unaffected.

## Hooks

Hooks deliver lifecycle events to HTTP endpoints as CloudEvents JSON:

```toml
[hook.archive]
url = "https://archive.example.internal/rushls"
events = ["segment.ready"]
signing_secret = { file = "/run/secrets/hook-signing" }
```

See [Admission and hooks](admission-and-hooks.md#lifecycle-hooks) for the events, payloads, and signature verification.

`segment.ready` fires for every completed segment in every rendition, with paths to fetch it from the origin.
It does not keep the segment available: fetch it before it leaves the DVR window. With six-second segments, one rendition produces about 600 events an hour.

A hook destination accepts `client_cert`, `client_key`, and `ca` for mutual TLS, the same as `[publish.auth]`.

## Authorization

`[publish.auth]` decides who may publish, and `[playback.auth]` who may watch. Without them, both are open.

### Publish

Rushls asks an HTTP service about each publisher when it connects:

```toml
[publish.auth]
url = "https://auth.example.com/v1/publish/admit"
token = { file = "/run/secrets/admission-token" }
timeout = "2s"
```

The service answers with the stream ID, the principal, and optionally a profile. A timeout, an error, or any unexpected response refuses the publisher.
See [Admission and hooks](admission-and-hooks.md#admission) for the request and response, and [examples/admission](../examples/admission) for a working service.

### Playback

Viewers present a signed JWT, which Rushls verifies itself without contacting another service per request.

```toml
[playback.auth]
jwks_url = "https://issuer.example.com/.well-known/jwks.json"
stream_claim = "stream"
leeway = "30s"

[playback.auth.claims]
iss = "https://issuer.example.com"
aud = "rushls-origin"
```

Use exactly one key source: `jwks_url` for a rotating key set, `public_key` for a single RS256 or ES256 key, or `secret` for HS256.
An asymmetric key is safer, because the origin then holds nothing that can create tokens.

`[playback.auth.claims]` lists claims the token must carry with exactly these values. `iss` and `aud` are required.
Add your own, such as `tier = "premium"`. `exp` is always required, and `leeway` applies to `exp` and `nbf`.
`stream_claim` names the claim holding the stream the token grants, so a token for one stream cannot open another.

A player sends the token as `Authorization: Bearer <jwt>` or as a `?token=<jwt>` query parameter.
Players that cannot set headers on every request use the query form. For them, Rushls serves playlists with
`#EXT-X-DEFINE:QUERYPARAM="token"`, which makes the player add the token to every URL it requests.
That needs HLS version 11 support in the player; hls.js-light does not have it.

With playback authorization on, responses drop `Cache-Control: public`. Query tokens appear in access and proxy logs, so keep them short-lived and scoped to one stream.
For CDN cache keys, see [Deployment](deployment.md#reverse-proxies-and-cdns) and the [Cloudflare example](../examples/cloudflare).

## Metrics and logging

```toml
[metrics]
listen = "127.0.0.1:9090"
token  = { file = "/run/secrets/metrics-token" }

[log]
level  = "info"   # error, warn, info, debug, or trace
format = "text"   # or "json", one object per line
```

Metrics are off until `[metrics]` is present. They get their own listener by default, because stream names are visible in the series.
To serve them on a viewer port instead, set `listen` to the same address as `[http]` or `[https]`, and set a token.
See [Metrics](metrics.md).

`log.level` sets how much Rushls itself logs; dependencies stay at `warn`.
`RUST_LOG`, when set, replaces the whole filter with its own directives, for example `RUST_LOG=info,quinn=trace` to debug QUIC.
It overrides the file and `RUSHLS_LOG_LEVEL`; `--log-level` overrides it.

## Node

```toml
name = "origin-eu-1"     # defaults to the hostname
shutdown_grace = "10s"
```

`name` identifies the node in logs, metrics, and hook events. Give each node behind a load balancer its own.

`shutdown_grace` is how long a stopping node waits for viewers on blocking playlist requests and for queued hook events before exiting.
Set it below your service manager's or orchestrator's stop timeout, so the node exits on its own rather than being killed.
