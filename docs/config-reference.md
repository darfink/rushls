# Configuration reference

Generated from the configuration schema; do not edit by hand. Regenerate with
`RUSHLS_UPDATE_CONFIG_REFERENCE=1 cargo test --lib config_reference`.

Values resolve defaults < TOML < environment < CLI. Credentials take a string,
`"${VAR}"`, or `{ file = "/path" }`. Their CLI flags take only a file path,
because arguments are visible in the process list. Structured values
(tables and lists) use TOML syntax in environment variables and flags too.
See [rushls.example.toml](../rushls.example.toml) for an annotated file.

## (top level)

| Setting | Default | Environment | CLI | Description |
|---|---|---|---|---|
| `name` | — | `RUSHLS_NAME` | `--name` | Identifies this node in logs, metrics, and hook events. |
| `shutdown_grace` | `10s` | `RUSHLS_SHUTDOWN_GRACE` | `--shutdown-grace` | How long a restart waits for parked viewer requests and queued hook events before exiting anyway. |

## [ingest]

| Setting | Default | Environment | CLI | Description |
|---|---|---|---|---|
| `ingest.idle_timeout` | `10s` | `RUSHLS_INGEST_IDLE_TIMEOUT` | `--ingest-idle-timeout` | How long an established connection may carry nothing before it is closed, or "off" on a trusted link. |
| `ingest.stall_timeout` | `12s` | `RUSHLS_INGEST_STALL_TIMEOUT` | `--ingest-stall-timeout` | How long a connected publisher may deliver no usable media before it is dropped, or "off". |
| `ingest.rtmp.listen` | `0.0.0.0:1935` | `RUSHLS_INGEST_RTMP_LISTEN` | `--ingest-rtmp-listen` | Address receiving RTMP publishers. |
| `ingest.srt.listen` | `0.0.0.0:9000` | `RUSHLS_INGEST_SRT_LISTEN` | `--ingest-srt-listen` | Address receiving SRT publishers. SRT currently requires IPv4. |
| `ingest.srt.latency` | `120ms` | `RUSHLS_INGEST_SRT_LATENCY` | `--ingest-srt-latency` | SRT receive latency; increase for unstable or long-distance networks. |
| `ingest.srt.passphrase` | — | `RUSHLS_INGEST_SRT_PASSPHRASE` | `--ingest-srt-passphrase <PATH>` | Optional passphrase: inline, `${VAR}`, or `{ file = "/path" }`. Absent accepts unencrypted SRT. |
| `ingest.srt.encryption` | `aes256` | `RUSHLS_INGEST_SRT_ENCRYPTION` | `--ingest-srt-encryption` | Encryption strength used when a passphrase is configured. |
| `ingest.moq.listen` | `off` | `RUSHLS_INGEST_MOQ_LISTEN` | `--ingest-moq-listen` | Address receiving WebTransport publishers, or "off". Needs `[tls]`: WebTransport has no cleartext form, which is why it is off by default. |

## [publish]

| Setting | Default | Environment | CLI | Description |
|---|---|---|---|---|
| `publish.strict` | `true` | `RUSHLS_PUBLISH_STRICT` | `--publish-strict` | Reject timing violations and dependent segment starts. False permits bounded, reported gap recovery. |
| `publish.takeover` | `false` | `RUSHLS_PUBLISH_TAKEOVER` | `--publish-takeover` | Whether a second publisher may replace the one holding a stream name. |
| `publish.rate` | — | `RUSHLS_PUBLISH_RATE` | `--publish-rate` | Media pace as multiples of wall clock: `{ max = "1x", burst = "10s", min = "0.5x", window = "30s" }`. |
| `publish.video` | — | `RUSHLS_PUBLISH_VIDEO` | `--publish-video` | Video predicates: codecs, resolution, frame_rate, tracks. |
| `publish.audio` | — | `RUSHLS_PUBLISH_AUDIO` | `--publish-audio` | Audio predicates: codecs, sample_rate, channels, tracks. |
| `publish.subtitles` | — | `RUSHLS_PUBLISH_SUBTITLES` | `--publish-subtitles` | Subtitle predicates: codecs, tracks. |
| `publish.profile` | — | — | — | Named alternatives, selected by an auth response `{"profile": "<name>"}`. |
| `publish.auth.url` | — | `RUSHLS_PUBLISH_AUTH_URL` | `--publish-auth-url` | Service asked to admit each publisher. |
| `publish.auth.timeout` | `2s` | `RUSHLS_PUBLISH_AUTH_TIMEOUT` | `--publish-auth-timeout` | Deadline for the whole call, connection included. |
| `publish.auth.max_response` | `64KiB` | `RUSHLS_PUBLISH_AUTH_MAX_RESPONSE` | `--publish-auth-max-response` | Largest decision this node will read. |
| `publish.auth.token` | — | `RUSHLS_PUBLISH_AUTH_TOKEN` | `--publish-auth-token <PATH>` | Bearer credential presented to the service: inline, `${VAR}`, or `{ file = "/path" }`. |
| `publish.auth.client_cert` | — | `RUSHLS_PUBLISH_AUTH_CLIENT_CERT` | `--publish-auth-client-cert` | Path to a PEM certificate chain this node presents to the service. |
| `publish.auth.client_key` | — | `RUSHLS_PUBLISH_AUTH_CLIENT_KEY` | `--publish-auth-client-key` | Path to the PEM private key for that chain. |
| `publish.auth.ca` | — | `RUSHLS_PUBLISH_AUTH_CA` | `--publish-auth-ca` | Path to a PEM authority to trust instead of the platform store. |

## [limits]

| Setting | Default | Environment | CLI | Description |
|---|---|---|---|---|
| `limits.publishers` | `256` | `RUSHLS_LIMITS_PUBLISHERS` | `--limits-publishers` | Concurrent ingest sessions. |
| `limits.streams` | `1024` | `RUSHLS_LIMITS_STREAMS` | `--limits-streams` | Streams held at once: live ones, plus ended ones still inside `hls.window`. When full, a new stream name is refused. |

## [memory]

| Setting | Default | Environment | CLI | Description |
|---|---|---|---|---|
| `memory.total` | `unlimited` | `RUSHLS_MEMORY_TOTAL` | `--memory-total` | Memory this node may commit across publishers and streams, or "unlimited". |
| `memory.per_publisher` | `128MiB` | `RUSHLS_MEMORY_PER_PUBLISHER` | `--memory-per-publisher` | Media one publisher holds in flight before storage, or "unlimited". Minimum 64MiB, which fits a maximum-sized packet and its output. |
| `memory.per_stream` | `512MiB` | `RUSHLS_MEMORY_PER_STREAM` | `--memory-per-stream` | Stored media and cached playlists for one stream. With `[disk]`, one eighth is kept for playlists and older media spills to disk. |

## [disk]

| Setting | Default | Environment | CLI | Description |
|---|---|---|---|---|
| `disk.per_stream` | — | `RUSHLS_DISK_PER_STREAM` | `--disk-per-stream` | Disk for one stream's older media. Setting it enables spilling; omit to stay in memory. |
| `disk.dir` | — | `RUSHLS_DISK_DIR` | `--disk-dir` | Directory for spilled media. Defaults to the platform cache. |

## [hls]

| Setting | Default | Environment | CLI | Description |
|---|---|---|---|---|
| `hls.scrubbing` | `true` | `RUSHLS_HLS_SCRUBBING` | `--hls-scrubbing` | Publish keyframe playlists for fast seeking and scrubbing through CMAF video. |
| `hls.segment` | `6s` | `RUSHLS_HLS_SEGMENT` | `--hls-segment` | Segment cadence: `"6s"`, or `{ target = "6s", max = "2x", tolerance = "0s" }`. |
| `hls.part` | `1s` | `RUSHLS_HLS_PART` | `--hls-part` | Part cadence: `"1s"`, or `{ target = "1s", max = "2x" }`. |
| `hls.window` | `6x` | `RUSHLS_HLS_WINDOW` | `--hls-window` | How much completed media each live playlist offers: the DVR window. |
| `hls.hold_back` | `3x` | `RUSHLS_HLS_HOLD_BACK` | `--hls-hold-back` | How far behind the live edge a player is told to start. |

## [http]

| Setting | Default | Environment | CLI | Description |
|---|---|---|---|---|
| `http.max_connections` | `4096` | `RUSHLS_HTTP_MAX_CONNECTIONS` | `--http-max-connections` | Maximum established HTTP connections across all HTTP listeners. |
| `http.max_requests` | `4096` | `RUSHLS_HTTP_MAX_REQUESTS` | `--http-max-requests` | Maximum HTTP requests executing or streaming responses across all listeners. |
| `http.listen` | `0.0.0.0:8080` | `RUSHLS_HTTP_LISTEN` | `--http-listen` | Address serving HLS and health probes, or `"off"` to serve HTTPS only. |
| `http.public_url` | `` | `RUSHLS_HTTP_PUBLIC_URL` | `--http-public-url` | Absolute URL prefix for the names playlists emit; empty is relative, which is right behind a proxy or CDN. |
| `http.cors.origins` | `*` | `RUSHLS_HTTP_CORS_ORIGINS` | `--http-cors-origins` | `*`, `off`, or a list of origins, each optionally starting with a wildcard label: `https://*.example.com` for one label, or `https://**.example.com` for any depth. |
| `http.cors.expose_headers` | `content-length,content-range,date` | `RUSHLS_HTTP_CORS_EXPOSE_HEADERS` | `--http-cors-expose-headers` | Response header names visible to cross-origin players. TOML uses an array; CLI and environment values use comma-separated names. Replaces the default list; an empty list exposes only browser-safelisted headers. |
| `http.cors.credentials` | `false` | `RUSHLS_HTTP_CORS_CREDENTIALS` | `--http-cors-credentials` | Permit cookies or browser authorization on cross-origin requests. |
| `http.cors.max_age` | `10min` | `RUSHLS_HTTP_CORS_MAX_AGE` | `--http-cors-max-age` | How long browsers may cache a successful CORS preflight. |

## [https]

| Setting | Default | Environment | CLI | Description |
|---|---|---|---|---|
| `https.version.min` | `1.3` | `RUSHLS_HTTPS_VERSION_MIN` | `--https-version-min` | Lowest accepted HTTPS version: "1.2" or "1.3". |
| `https.version.max` | `1.3` | `RUSHLS_HTTPS_VERSION_MAX` | `--https-version-max` | Highest accepted HTTPS version: "1.2" or "1.3". |
| `https.listen` | `[::]:8443` | `RUSHLS_HTTPS_LISTEN` | `--https-listen` | Address serving HTTPS, bound independently of the cleartext listener. |
| `https.handshake_timeout` | `5s` | `RUSHLS_HTTPS_HANDSHAKE_TIMEOUT` | `--https-handshake-timeout` | Bounds a connection that completes TCP and then stalls mid-handshake. |
| `https.max_handshakes` | `256` | `RUSHLS_HTTPS_MAX_HANDSHAKES` | `--https-max-handshakes` | Handshakes admitted at once, which is what stops a flood from growing the task set without bound. |

## [tls]

| Setting | Default | Environment | CLI | Description |
|---|---|---|---|---|
| `tls.cert` | — | `RUSHLS_TLS_CERT` | `--tls-cert` | Path to a PEM certificate chain, leaf first. Reloaded on rotation. |
| `tls.key` | — | `RUSHLS_TLS_KEY` | `--tls-key` | Path to the PEM private key for that chain. |

## [playback]

| Setting | Default | Environment | CLI | Description |
|---|---|---|---|---|
| `playback.auth.public_key` | — | `RUSHLS_PLAYBACK_AUTH_PUBLIC_KEY` | `--playback-auth-public-key` | RS256/ES256 verifying key: inline PEM or `{ file = "/path" }`. |
| `playback.auth.jwks_url` | — | `RUSHLS_PLAYBACK_AUTH_JWKS_URL` | `--playback-auth-jwks-url` | Issuer key set fetched when the gate starts. |
| `playback.auth.secret` | — | `RUSHLS_PLAYBACK_AUTH_SECRET` | `--playback-auth-secret <PATH>` | HS256 shared secret: inline, `${VAR}`, or `{ file = "/path" }`. |
| `playback.auth.stream_claim` | `stream` | `RUSHLS_PLAYBACK_AUTH_STREAM_CLAIM` | `--playback-auth-stream-claim` | Claim carrying the stream the token admits. |
| `playback.auth.leeway` | `30s` | `RUSHLS_PLAYBACK_AUTH_LEEWAY` | `--playback-auth-leeway` | Clock skew allowed on `exp` and `nbf`. |
| `playback.auth.claims` | — | — | — | Claims that must be present with these values; `iss` and `aud` are required. |

## [metrics]

| Setting | Default | Environment | CLI | Description |
|---|---|---|---|---|
| `metrics.listen` | — | `RUSHLS_METRICS_LISTEN` | `--metrics-listen` | Where metrics are served. Absent exports nothing. |
| `metrics.token` | — | `RUSHLS_METRICS_TOKEN` | `--metrics-token <PATH>` | Bearer token required to scrape `/metrics` and `/metrics/streams`: inline, `${VAR}`, or `{ file = "/path" }`. |

## [record]

| Setting | Default | Environment | CLI | Description |
|---|---|---|---|---|
| `record` | — | — | — | Persistent local segment exports, independent of DVR retention. |
| `record.dir` | — | — | — | Directory recordings are written under. Setting the table enables recording. |
| `record.path` | `{stream}/{publication}/{time:%Y/%m/%d}/{rendition}_{segment}.mp4` | — | — | Where each completed segment lands under `dir`. |
| `record.queue_size` | `128` | — | — | Segments queued for writing before new ones are dropped. |
| `record.max_pending` | `256MiB` | — | — | Open segments, queued jobs, and the active write together. |

## [hook]

| Setting | Default | Environment | CLI | Description |
|---|---|---|---|---|
| `hook` | — | — | — | Hook destinations, keyed by the name that identifies each in logs and metrics. |
| `hook.<name>.url` | — | — | — | Where deliveries are posted. |
| `hook.<name>.events` | — | — | — | Events this destination receives; required. |
| `hook.<name>.token` | — | — | — | Bearer credential: inline, `${VAR}`, or `{ file = "/path" }`. |
| `hook.<name>.signing_secret` | — | — | — | `whsec_` key signing each delivery. |
| `hook.<name>.queue_size` | `1000` | — | — | Events held before the oldest is dropped. |
| `hook.<name>.max_in_flight` | `8` | — | — | Distinct streams delivered at once. |
| `hook.<name>.max_attempts` | `5` | — | — | Attempts per event, the first included. |
| `hook.<name>.client_cert` | — | — | — | PEM certificate chain presented to this endpoint. |
| `hook.<name>.client_key` | — | — | — | PEM private key for that chain. |
| `hook.<name>.ca` | — | — | — | PEM authority to trust instead of the platform store. |
