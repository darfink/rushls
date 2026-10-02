# Deploying Rushls

This guide covers one origin instance. Capacity depends on media, workload, storage, and player behavior.
Use measured limits for your deployment; the examples are starting values.

## Configuration and network boundaries

Start with the annotated configuration from the installed binary:

```sh
rushls --print-config-example > production.toml
```

The generated file binds to loopback and leaves publishing and playback unauthenticated.
Configure admission, playback access, listener addresses, certificates, and capacity before exposing the origin.
Each TOML snippet in this guide is a fragment. Merge tables into one configuration instead of repeating table headers.
Use `--config` to select that file explicitly.

| Listener | Transport | Example port | Encryption |
| --- | --- | --- | --- |
| RTMP | TCP | 1935 | None; use RTMPS |
| RTMPS | TCP | 1936 | Native TLS 1.2 or 1.3 |
| SRT | UDP, IPv4 only | 9000 | SRT passphrase / AES |
| MoQ | UDP | 4433 | QUIC TLS 1.3 |
| HTTP | TCP | 8080 | Cleartext or a private proxy backend |
| HTTPS | TCP | 8443 | Native TLS |
| Metrics | TCP | 9090 | Private listener and bearer token |

Only publish the ports you use. Docker requires explicit UDP mappings for SRT and MoQ.
A normal HTTP reverse proxy does not proxy RTMP, SRT, or MoQ ingestion.
Neither RTMP nor SRT accepts `listen = "off"`; use network isolation when a listener is unused.
MoQ stays disabled without its certificate configuration. HTTP can use `listen = "off"`.

## Storage and capacity

DVR storage and recording have separate lifetimes:

| Storage | Purpose | Restart behavior | Cleanup |
| --- | --- | --- | --- |
| Memory retention | Recent media and manifests | Lost | Time/byte retention |
| Disk retention | Older media in the live DVR window | Catalog is not restored | Owned-generation cleanup and retention |
| Recording directory | Persistent completed segments | Files remain | External archive policy |

Example for a two-hour DVR window:

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

Estimate media bytes as total encoded bitrate × seconds ÷ 8.
A 6 Mbps publication needs about 5.4 GB for two hours, plus overhead.
A ladder's bitrate is the sum of all video and audio tracks retained by the origin.
Allow space for bitrate peaks, manifests, temporary writes, and other filesystem users.

The example permits up to 32 retained streams, so disk budgets can approach 256 GiB in aggregate.
Sixteen active publishers do not limit retained idle streams to sixteen.
Memory budgets exclude some pipeline and in-flight allocations and do not cap process RSS.
Measure actual retention depth and memory under the expected workload.

Use a distinct DVR root per origin process. Ownership locks prevent concurrent Rushls instances from sharing that root.
Keep recordings outside the DVR root. Do not treat a mounted DVR volume as restart-persistent playback state.

The container runs as UID/GID `65532:65532`.
For Linux bind mounts, create the directories and grant that identity access:

```sh
sudo install -d -o 65532 -g 65532 /srv/rushls/dvr /srv/rushls/recordings
```

Mount `/srv/rushls` at `/var/lib/rushls` and mount secrets read-only at `/run/secrets`.
The identity also needs permission to traverse parent directories and read certificates and secret files.
On Windows, use a local filesystem with hard links, such as NTFS.
Do not assume a network filesystem provides the same lock and durability behavior.

## Recording operations

```toml
[record]
dir = "/var/lib/rushls/recordings"
path = "{stream}/{publication}/{time:%Y/%m/%d}/{rendition}_{segment}.mp4"
queue_size = 128
max_pending = "256MiB"
```

The recorder publishes completed segment files without replacing existing names.
Each file carries decoder configuration, but codec preroll and source random access still affect standalone decoding.
Keep publication identity in the path to separate sessions.

Rushls does not provide an archive playback API or an automatic recording expiry policy.
Use a separate process to index, upload, and expire completed files.
Do not consume temporary files as completed recordings.

Watch recording failures and queue pressure. Live playback can continue while the recorder drops work after an error or overflow.
For remote archival, `segment.ready` hooks supply fetchable resource paths instead of media payloads.
Fetch them before live retention expires. A hook does not reserve the segment indefinitely.

Unix flushes file contents and directory metadata. Windows flushes contents without the same directory-entry power-loss guarantee.
See [Windows notes](installation.md#windows).

## Authorization

Publisher admission is an application service, not a list of passwords in Rushls:

```toml
[publish.auth]
url = "https://auth.example.com/v1/publish/admit"
token = { file = "/run/secrets/admission-token" }
timeout = "2s"
max_response = "64KiB"
```

The admission response chooses `stream_id`, `principal`, and an optional named profile.
Failures deny the connection. A reconnect makes a new admission request.
The shared token authenticates Rushls to the service; it is not the publisher's stream key.
Use the exact [admission](admission-and-hooks.md#admission) contract.

For playback, choose one key source: a public key, JWKS URL, or HMAC secret.
This example uses JWKS:

```toml
[playback.auth]
jwks_url = "https://issuer.example.com/.well-known/jwks.json"
stream_claim = "stream"
leeway = "30s"

[playback.auth.claims]
iss = "https://issuer.example.com"
aud = "rushls-origin"
```

Viewer JWTs need the stream claim, expiry, and required issuer/audience claims.
Send `Authorization: Bearer TOKEN`, or use the supported `token` query parameter for players that cannot set headers.
Query tokens can appear in access logs. Restrict log access and redact credentials in surrounding infrastructure.

Bearer authentication needs headers on subsequent media requests, not only the first playlist request.
Query-token playlists require HLS v11 `EXT-X-DEFINE:QUERYPARAM` substitution, including on child requests.
Players without this support, including hls.js-light, cannot use that form. See [playback authorization](configuration.md#playback) before integrating a player or CDN.
For JWKS deployments, allow outbound access to the issuer and monitor key refresh failures.

## Encryption and certificates

Native HTTPS configuration:

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

Set `min = "1.2"` when older clients require TLS 1.2. Keep `max = "1.3"` to permit modern negotiation.
The certificate must cover the public hostname and provide the required chain.
An external certificate manager must issue and renew it.
Rushls reloads valid certificate replacements; an invalid replacement leaves the previous certificate active.
Mount certificate directories so replacement files become visible inside the container.

RTMPS is served natively with the same `[tls]` certificate:

```toml
[ingest.rtmps]
listen = "0.0.0.0:1936"

[tls]
cert = "/run/secrets/fullchain.pem"
key = "/run/secrets/private-key.pem"
```

Encoders publish to `rtmps://origin.example.com:1936/live/NAME`. The listener accepts TLS 1.2 and 1.3.
To stop accepting unencrypted RTMP, keep `ingest.rtmp.listen` on loopback or block its port.

An external TCP TLS terminator in front of the plain RTMP listener also works.
Do not put RTMP through an HTTP `proxy_pass` block.
Behind a terminator, every RTMP publisher appears to come from the terminator's address.
Configure it to send a PROXY protocol header (v1 or v2) and enable `proxy_protocol`, so admission, logs, hooks, and per-address limits see the real client:

```toml
[ingest.rtmp]
listen = "127.0.0.1:1935"
proxy_protocol = true
```

With HAProxy, add `send-proxy-v2` to the `server` line. With nginx `stream`, set `proxy_protocol on;`. AWS NLB target groups have a proxy protocol v2 attribute.
Once enabled, a connection without a header is refused, so the listener must be reachable only through the proxy.
MoQ ingest reuses the `[tls]` certificate; QUIC always requires TLS 1.3.

SRT uses separate encryption:

```toml
[ingest]
idle_timeout = "5s"

[ingest.srt]
listen = "0.0.0.0:9000"
passphrase = { file = "/run/secrets/srt-passphrase" }
encryption = "aes256"
latency = "120ms"
```

SRT passphrases must satisfy the transport's 10–79-byte requirement.
Configure a matching publisher passphrase and AES key size.
Keep the idle timeout longer than the transport latency.
Do not confuse encryption with publisher authorization.

## Reverse proxies and CDNs

For external HTTPS termination, keep the HTTP backend private:

```toml
[http]
listen = "127.0.0.1:8080"
public_url = "https://video.example.com"

[http.cors]
origins = ["https://player.example.com"]
```

An Nginx location inside an HTTPS server can forward playback requests:

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

This fragment assumes an existing HTTPS virtual host and certificate configuration.
It preserves the original URI and query string and disables proxy caching and buffering.
Set timeouts above the origin's blocking-reload deadline, with network margin.

Before adding a CDN cache, verify these behaviors:

- Preserve `_HLS_msn`, `_HLS_part`, and `_HLS_skip` query parameters and distinguish their responses.
- Forward credentials and preserve the origin's authorization and cache-control semantics.
- Do not share playlists containing one viewer's token with other viewers.
- Support Range requests and the origin's content types and CORS headers.
- Honor `no-store` failures instead of caching transient missing media or overload responses.
- Keep blocked playlist requests open and permit clients to fetch parts promptly.

`public_url` controls advertised URLs. It does not configure DNS, TLS, or a reverse proxy.
Relative URLs remain the default when it is empty.

## Reconnects and shutdown

Strict input validation is the default. It rejects invalid timing and dependent segment starts.
Permissive input mode only handles the bounded cases described in [input handling](input-handling.md).
It does not generate missing audio or video.

`publish.takeover = true` lets a new publisher replace an active publisher with the same admitted stream ID.
It does not combine those publishers into a multitrack presentation or replicate state across origins.
Reconnect continuity depends on the retained stream and compatible media.

For SRT, an orderly peer shutdown closes the input and can produce `ENDLIST`.
Idle or sequence breaks count as interruptions and preserve a reconnect opportunity within retention.
RTMP distinguishes explicit unpublish from transport loss.
Monitor stream lifecycle events instead of treating every disconnect as an identical event.

Set a finite shutdown budget:

```toml
shutdown_grace = "10s"
```

Send SIGTERM on Unix or Ctrl+C/Ctrl+Break on Windows.
Allow more than ten seconds in the service manager or container orchestrator for this example.
A forced kill bypasses normal draining.

A minimal Linux systemd service, after installing the binary and creating the `rushls` user, is:

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

Grant this service user access to the configured directories and secrets.
The native service identity differs from the container's numeric identity.
Restart the service after configuration changes; there is no general configuration reload endpoint.

## Monitoring and rollout

Use `/health/live` and `/health/ready` on the HTTP(S) listener for probes.
Readiness is an origin readiness check, not proof that a particular stream exists or decodes correctly.
If an RTMP, RTMPS, HTTP, or HTTPS listener keeps failing to accept connections for 30 seconds, for example because the process has run out of file descriptors, readiness fails with `accept failing`.
It passes again after the next successful accept, or after 10 seconds without a failure.
Liveness does not change, because a restart would cut every live session; the node recovers as sessions end and free descriptors.
A listening socket that is itself unusable stops the node with an error instead.
Restrict metrics access with a private listener and bearer token.
Start with [metrics documentation](metrics.md) and the [Prometheus/Grafana examples](../examples/monitoring).

Track admission failures, timestamp failures, stream availability, retention depth, recording failures, hook delivery, CPU, memory, and filesystem capacity.
Test a representative publisher and player through the actual proxy/CDN before rollout.
Include reconnects, disk pressure, and viewer load in operational validation.
See [load validation](development/load-testing.md) for the measured harness and its limits.
