# Deploying Rushls

This guide covers running a Rushls origin in production: ports, storage, recordings, authorization, TLS, proxies, CDNs, systemd, Kubernetes, and health probes.
All limits below are starting points—measure actual memory, disk, and bandwidth under your own encoders and viewers.

## Configuration and network boundaries

Generate the annotated configuration from the binary you are deploying:

```sh
rushls --print-config-example > production.toml
```

Out of the box, that file binds to loopback and leaves publishing and playback open.
Before exposing the origin, configure admission, playback authorization, listener addresses, TLS certificates, and capacity limits, then validate with `rushls --config production.toml --check`.
The TOML snippets below are fragments—merge their tables into one file rather than repeating headers.

| Listener | Transport | Example port | Encryption |
| --- | --- | --- | --- |
| RTMP | TCP | 1935 | None (prefer RTMPS or a TLS terminator) |
| RTMPS | TCP | 1936 | Native TLS 1.2 or 1.3 |
| SRT | UDP (IPv4 only) | 9000 | SRT passphrase / AES |
| MoQ | UDP | 4433 | QUIC TLS 1.3 |
| HTTP | TCP | 8080 | Cleartext (keep behind a proxy, or turn off) |
| HTTPS | TCP | 8443 | Native TLS 1.2 or 1.3 |
| Metrics | TCP | 9090 | Private listener and bearer token |

Only expose the ports you actually use (Docker requires explicit `/udp` mappings for SRT and MoQ, and standard HTTP reverse proxies cannot proxy RTMP, SRT, or MoQ).
RTMP and SRT do not accept `listen = "off"`, so keep unused ingest listeners bound to loopback or blocked by your firewall.
MoQ stays off unless configured with `[tls]`, and HTTP can be turned off with `http.listen = "off"` when serving HTTPS directly.

## Storage and capacity

DVR retention and segment recording serve different jobs and have different lifetimes:

| Storage | Purpose | After a restart | Cleanup |
| --- | --- | --- | --- |
| Memory (`[memory]`) | Recent media and cached playlists | Cleared | Time and byte budgets |
| Disk spill (`[disk]`) | Older media in the live DVR window | Cleared (old files removed) | Retention window and byte budgets |
| Recording (`[record]`) | Permanent completed segments | Preserved on disk | External archive/expiry job |

A starting configuration for a two-hour DVR window:

```toml
[hls]
window = "2h"

[limits]
publishers = 16
streams = 32

[memory]
per_stream = "256MiB"

[disk]
per_stream = "8GiB"
dir = "/var/lib/rushls/dvr"
```

Estimate disk per stream as `total bitrate × seconds ÷ 8`, summing every video and audio rendition in the ladder.
A 6 Mbps ladder over two hours takes roughly 5.4 GB before headroom for bitrate spikes and temporary files.
Because `limits.streams = 32` keeps ended streams around until their window expires, this example can commit up to 256 GiB of disk even with only 16 concurrent publishers.
Memory budgets cap stored media and accounted pipeline buffers, not total process RSS, so leave room for the runtime, OS buffers, and allocator overhead.

Keep each Rushls process on its own `disk.dir` (Rushls locks the directory so two instances cannot share it), and keep `record.dir` outside `disk.dir`.
Never treat a DVR volume as restart-persistent playback storage.

For containers, the image runs as UID/GID `65532:65532`. On Linux hosts, prepare bind-mounted directories for that user:

```sh
sudo install -d -o 65532 -g 65532 /srv/rushls/dvr /srv/rushls/recordings
```

Mount `/srv/rushls` at `/var/lib/rushls` and mount secrets read-only at `/run/secrets`, ensuring UID `65532` can traverse parent directories and read certificates and secret files.
Use a local filesystem that supports hard links and reliable file locks (such as ext4, XFS, APFS, or NTFS on Windows) rather than a network filesystem.

## Recording operations

```toml
[record]
dir = "/var/lib/rushls/recordings"
path = "{stream}/{publication}/{time:%Y/%m/%d}/{rendition}_{segment}.mp4"
queue_size = 128
max_pending = "256MiB"
```

Rushls writes each completed segment atomically—in-progress files use `.rushls-*.tmp` names and never overwrite existing files.
Keep `{publication}` in `path` so reconnects and restarts never collide.
Each MP4 segment carries its own initialization header, though codec preroll and source random-access structure still apply if you decode segments individually.

Rushls does not index, serve, upload, or expire recordings. Run a separate job (such as `rclone move --exclude '.rushls-*' --min-age 1m`) to archive and prune completed files.
Watch recording failure and backlog metrics: if `queue_size` or `max_pending` overflows, Rushls drops affected recording segments so live playback stays uninterrupted.

If you archive remotely instead of writing to local disk, subscribe a [hook](admission-and-hooks.md#lifecycle-hooks) to `segment.ready` and fetch each segment over HTTP before it leaves the DVR window.

## Authorization

Publisher admission delegates allow/deny decisions to your own HTTP service rather than storing static stream keys in Rushls:

```toml
[publish.auth]
url = "https://auth.example.com/v1/publish/admit"
token = { file = "/run/secrets/admission-token" }
timeout = "2s"
max_response = "64KiB"
```

The `token` here authenticates Rushls to your admission service (it is not a publisher's stream key).
Your service returns `stream_id`, `principal`, and an optional named profile; any network error, timeout, or invalid response fails closed and denies the publisher. Every reconnect triggers a fresh check.
See [Admission and hooks](admission-and-hooks.md#admission) for the JSON schema and [`examples/admission`](../examples/admission/admission.py) for a starter service.

Viewer playback authorization verifies signed JWTs locally using one key source (`jwks_url`, `public_key`, or `secret`):

```toml
[playback.auth]
jwks_url = "https://issuer.example.com/.well-known/jwks.json"
stream_claim = "stream"
leeway = "30s"

[playback.auth.claims]
iss = "https://issuer.example.com"
aud = "rushls-origin"
```

Viewers pass the JWT via `Authorization: Bearer <token>` (on every playlist and media request) or via `?token=<token>`.
Query tokens rely on HLS v11 `EXT-X-DEFINE:QUERYPARAM` substitution—players without it (including `hls.js-light`) cannot use query tokens—and they can appear in proxy and CDN access logs, so keep them short-lived, scoped to one stream, and redacted in surrounding logs.
When using `jwks_url`, allow outbound access from Rushls to the issuer and alert on key refresh failures.
See [Playback authorization](configuration.md#playback) for full details.

## Encryption and certificates

Serve HTTPS directly with native TLS (including HTTP/2 via ALPN):

```toml
[http]
listen = "off"

[https]
listen = "0.0.0.0:8443"
handshake_timeout = "5s"
max_handshakes = 256

[tls]
cert = "/run/secrets/fullchain.pem"
key = "/run/secrets/private-key.pem"
```

HTTPS defaults to TLS 1.3 only; set `min_version = "1.2"` under `[https]` if older clients need TLS 1.2 (TLS 1.3 stays accepted either way).
Issue and renew certificates externally—Rushls watches the files and hot-reloads valid replacements, keeping the previous certificate active if a replacement is invalid.
In containers, mount the parent directory rather than individual symlinked files so atomic renewals become visible inside the container.

The same `[tls]` certificate serves RTMPS (TLS 1.2 and 1.3) and MoQ (QUIC TLS 1.3):

```toml
[ingest.rtmps]
listen = "0.0.0.0:1936"

[tls]
cert = "/run/secrets/fullchain.pem"
key = "/run/secrets/private-key.pem"
```

Encoders publish to `rtmps://origin.example.com:1936/live/NAME`. To prevent unencrypted RTMP, bind `ingest.rtmp.listen` to loopback or block port 1935 at the firewall.

If you terminate TLS in front of plain RTMP (or pass TLS through a TCP load balancer to RTMPS), do not route RTMP through an HTTP `proxy_pass` block.
Instead, use a TCP proxy that sends PROXY protocol v1 or v2 and enable `proxy_protocol` so admission, logs, hooks, and `limits.publishers_per_address` see the real client IP:

```toml
[ingest.rtmp]
listen = "127.0.0.1:1935"
proxy_protocol = true
```

In HAProxy, add `send-proxy-v2` to the `server` line; in Nginx `stream`, set `proxy_protocol on;`; on AWS NLB, enable proxy protocol v2 on the target group.
Once enabled, any connection without a PROXY header is rejected, so keep the listener unreachable except through the proxy.

SRT uses its own passphrase encryption (independent of publisher admission):

```toml
[ingest]
idle_timeout = "5s"

[ingest.srt]
listen = "0.0.0.0:9000"
passphrase = { file = "/run/secrets/srt-passphrase" }
encryption = "aes256"
latency = "120ms"
```

SRT passphrases must be 10–79 bytes long, must match the publisher's passphrase and AES key length, and `ingest.idle_timeout` must stay longer than `ingest.srt.latency`.

## Reverse proxies and CDNs

When terminating HTTPS in front of Rushls, keep the HTTP listener private and set `http.public_url` only if you want absolute URLs in playlists (leaving it empty emits relative URLs, which works cleanly behind proxies and CDNs):

```toml
[http]
listen = "127.0.0.1:8080"
public_url = "https://video.example.com"

[http.cors]
origins = ["https://player.example.com"]
```

An Nginx location inside your HTTPS `server` block:

```nginx
location / {
    proxy_pass http://127.0.0.1:8080;
    proxy_http_version 1.1;
    proxy_set_header Host $host;
    proxy_set_header Authorization $http_authorization;
    proxy_buffering off;
    proxy_cache off;
    proxy_read_timeout 60s;
}
```

Keep `proxy_read_timeout` comfortably above your blocking playlist reload window, and keep buffering off so LL-HLS parts reach players immediately.

Before putting a CDN in front of Rushls, verify that it:

- Keeps `_HLS_msn`, `_HLS_part`, and `_HLS_skip` in the cache key.
- Honors Rushls's `Cache-Control` headers, including `no-store` on transient errors.
- Holds blocking playlist reloads open without buffering or timing them out early.
- Supports HTTP `Range` requests (needed for I-frame scrubbing) and forwards CORS headers.
- Never shares a viewer's token-bearing playlist with other viewers, and verifies JWTs at the edge if you cache media without tokens (see [`examples/cloudflare/playback-worker.js`](../examples/cloudflare/playback-worker.js)).

## Reconnects and shutdown

Strict input validation is on by default; see [Input handling](input-handling.md) before enabling permissive gap recovery (`publish.strict = false`).

Setting `publish.takeover = true` allows a reconnecting publisher with the same admitted `stream_id` to displace an existing session immediately instead of waiting for `ingest.stall_timeout`.
Takeover replaces the active publisher on the same node; it never merges publishers into one ladder or replicates state across nodes.
Across protocols, Rushls distinguishes clean shutdowns from drops: an explicit RTMP unpublish or orderly SRT close ends the playlist with `EXT-X-ENDLIST`, while a transport drop leaves the stream open for a compatible reconnect within `hls.window`.

Configure a finite drain budget for graceful stops:

```toml
shutdown_grace = "10s"
```

Send `SIGTERM` on Unix (or Ctrl+C / Ctrl+Break on Windows), and set your service manager's stop timeout higher than `shutdown_grace` so parked viewer requests, recordings, and queued hooks finish draining before a hard kill.

A minimal Linux systemd unit (after installing the binary and creating a `rushls` system user with access to your config, secrets, and storage directories):

```ini
[Unit]
Description=Rushls HLS origin
After=network-online.target
Wants=network-online.target

[Service]
User=rushls
Group=rushls
ExecStart=/usr/local/bin/rushls --config /etc/rushls/production.toml
Restart=on-failure
TimeoutStopSec=20
UMask=0027

[Install]
WantedBy=multi-user.target
```

There is no live config-reload endpoint; restart the service after editing configuration files (TLS certificates and JWKS keys still reload automatically).

## Kubernetes

[`examples/kubernetes/rushls.yaml`](../examples/kubernetes/rushls.yaml) deploys one origin with a ConfigMap, Deployment, LoadBalancer Service for RTMP/SRT ingest, and ClusterIP Service for viewers and metrics:

```sh
kubectl create secret generic rushls --from-literal=metrics-token="$(openssl rand -hex 32)"
kubectl apply -f examples/kubernetes/rushls.yaml
```

Key design choices in the manifest:

- **One replica (`Recreate` strategy):** Streams live on the origin their publisher connected to; running two replicas behind the same Service would return 404s for streams hosted on the other pod. On updates, the old pod drains and publishers reconnect to the new one.
- **Health probes:** Startup and readiness probes hit `/health/ready`; liveness hits `/health/live`.
- **Graceful shutdown:** A 5-second `preStop` sleep lets load balancers observe failed readiness before Rushls stops accepting connections, and `terminationGracePeriodSeconds` covers `preStop` plus `shutdown_grace`.
- **Memory headroom:** `memory.total` sits below the container memory limit so Rushls refuses excess streams cleanly instead of being OOM-killed.
- **Real publisher IPs:** `externalTrafficPolicy: Local` preserves publisher source addresses for admission and `limits.publishers_per_address` (behind a proxy that rewrites addresses, enable [`proxy_protocol`](#encryption-and-certificates) instead).
- **Hardening:** Runs as UID `65532` with a read-only root filesystem and an `emptyDir` at `/var/lib/rushls` (swap in a PersistentVolumeClaim if recordings must survive pod restarts).

Add `[publish.auth]` before exposing the ingest Service, serve viewers through an Ingress or CDN, and roll config changes with `kubectl rollout restart deploy/rushls`.

## Monitoring and rollout

Point load-balancer and orchestrator probes at `/health/live` and `/health/ready` on the HTTP(S) listener.
`/health/ready` verifies origin health—not whether any individual stream exists.
If an RTMP, RTMPS, HTTP, or HTTPS listener fails to accept connections continuously for 30 seconds (for example after exhausting file descriptors), readiness fails with `accept failing`, then recovers after the next successful accept or 10 seconds without failures.
Liveness stays green so active broadcasts are not cut while existing connections drain; a genuinely broken listening socket stops the process with an error instead.

Keep `[metrics]` on a private listener with a bearer token, and start with the [metrics guide](metrics.md) and [Grafana dashboards](../examples/monitoring/README.md).
Before going live, test a real publisher and player through your actual proxy or CDN—including reconnects, disk spill, and viewer concurrency.
See [Load testing](development/load-testing.md) for the soak harness and its scope.
