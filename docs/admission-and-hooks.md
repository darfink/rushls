# Admission and hooks

Rushls integrates with your application through two HTTP interfaces: **admission** (deciding whether a publisher may connect, and under which stream ID and profile) and **lifecycle hooks** (receiving CloudEvents as publishers connect, streams go live, segments finish, and broadcasts end).

Both share the same publisher identity shape: `protocol`, `resource`, and `client` describe what connected, while `stream_id` and `principal` record your authorization decision.

## Admission

When `[publish.auth]` is configured, Rushls sends a `POST` request with `Content-Type: application/json` for every incoming publisher:

```json
{
  "version": 1,
  "request_id": "0198abc0-0000-7000-8000-000000000001",
  "protocol": "srt",
  "resource": {"namespace": "organization/live", "name": "camera"},
  "credential": {"encoding": "base64", "value": "c2VjcmV0"},
  "client": {
    "remote_address": "[2001:db8::1]:50000",
    "encoder": null,
    "protocol_version": null
  }
}
```

- `protocol` is `rtmp`, `rtmps`, `srt`, or `moq` (`rtmps` means Rushls itself terminated TLS; plain RTMP behind an external TLS terminator reports `rtmp`).
- `remote_address` is the peer socket address and port (or the address from the PROXY protocol header when `proxy_protocol = true`). It cannot identify a publisher behind an upstream relay.
- `resource.namespace` and optional `client` metadata fields are `null` when unavailable.
- `request_id` identifies this admission check only; once admitted, the publishing session gets its own `session_id`.

Return HTTP `200` with a JSON decision matching the request's `version`. To allow the publisher:

```json
{
  "decision": "allow",
  "stream_id": "events/main",
  "principal": "account-42",
  "profile": "premium"
}
```

`stream_id` and `principal` are required non-blank strings. `profile` is optional and selects a `[publish.profile.NAME]` table; omitting it or setting `null` applies the default `[publish]` settings.

To deny the publisher:

```json
{"decision": "deny", "reason": "subscription_inactive"}
```

`reason` is optional human-readable context for your logs and is never sent back to the publisher.

Unknown fields are rejected in both responses, and a response cannot override `protocol`, `resource`, or `client`.
Admission always fails closed: transport errors, timeouts, non-2xx statuses, invalid JSON, unknown profiles, or invalid identities deny the connection without retrying. A reconnecting encoder triggers a new admission request.

## Lifecycle hooks

Configured `[hook.<name>]` endpoints receive `POST` requests with `Content-Type: application/cloudevents+json` for each event listed in `events`.

Events track two separate lifetimes:

- **`publisher.*` events** follow a single publisher connection from admission until it stops, identified by `session_id`.
- **`stream.*` events** follow what viewers can play. A stream outlives a disconnected publisher for the length of `hls.window`, so an encoder can reconnect under a new `session_id` while the stream stays available.

| Event | Sent when |
| --- | --- |
| `publisher.started` | A publisher passes [admission](#admission) and claims the stream, before media arrives (rejected publishers emit no events) |
| `stream.available` | The first playlist a viewer can load is ready; sent once per stream, not on reconnects |
| `segment.ready` | Every completed segment of every rendition is ready to fetch |
| `publisher.degraded` | Rushls first repairs a hole on one of this publisher's tracks (see [Input handling](input-handling.md#hooks-logs-and-metrics)) |
| `publisher.recovered` | A degraded track has carried 30 seconds of clean media |
| `publisher.ended` | The publisher stops for any reason (clean end, drop, takeover, shutdown, or failure); viewers can still play retained media |
| `stream.unavailable` | The stream has had no publisher for `hls.window` and is retired (viewers get 404); never sent for a stream that never became available |

A typical broadcast emits `publisher.started` → `stream.available` → `segment.ready` (repeated) → `publisher.ended` → `stream.unavailable`.
Events for a given stream are delivered in order.
To react when a broadcast is truly over, listen for `stream.unavailable` rather than `publisher.ended`, which also fires when an encoder drops and reconnects.

### Credential redaction and public stream IDs

Credentials never appear in lifecycle hooks:

- Admission receives the original credential and resource, while publisher hooks replace any `resource.name` containing the credential with `[redacted]` and omit a credential-bearing `resource.namespace`.
- **Return a public `stream_id` and `principal` that do not contain the publish credential** if you want lifecycle hooks. If either contains the credential—including with open RTMP admission, where the publish name becomes the playback path—Rushls withholds publisher hooks and stream URLs (logged as `hooks_withheld = true` on session acceptance). Playback itself still works, and reconnects inherit the restriction until the stream expires. If an earlier publisher made the stream public, subscribers still receive its `stream.unavailable`.

### Event format and versioning

CloudEvents `type` includes a schema version (`rushls.<kind>.v1`). Additive fields keep `.v1`, so consumers should ignore unrecognized fields; breaking changes (removing, renaming, or changing the meaning of a field) bump to `.v2`. Subscriptions name the kind without a version (such as `publisher.started`) and receive every version.

Example `publisher.started` event:

```json
{
  "specversion": "1.0",
  "id": "0198abc0-0000-7000-8000-000000000002",
  "source": "origin-1",
  "type": "rushls.publisher.started.v1",
  "subject": "events/main",
  "time": "2026-09-11T12:00:00Z",
  "datacontenttype": "application/json",
  "data": {
    "stream_id": "events/main",
    "session_id": "42",
    "principal": "account-42",
    "protocol": "srt",
    "resource": {"namespace": "organization/live", "name": "camera"},
    "client": {
      "remote_address": "[2001:db8::1]:50000",
      "encoder": null,
      "protocol_version": null
    }
  }
}
```

`publisher.ended` includes those same identity fields plus:

```json
{
  "outcome": "ended",
  "duration_ms": 180000,
  "was_available": true,
  "diagnostic": null
}
```

- `outcome` is one of `ended`, `interrupted`, `replaced`, `cancelled`, `unhealthy`, or `failed`.
- `diagnostic` is optional human-readable detail (not a stable error code).
- `was_available` indicates whether this publisher reached its running pipeline state; stream playability is reported by `stream.available`.

Stream events carry `stream_id` in `data` and omit publisher details, since a stream can span multiple publisher sessions. `stream.available` adds `playlist_path` (such as `/live/camera/index.m3u8`), which—like paths in `segment.ready`—is rooted rather than absolute so you can join it to your origin or CDN host.

`session_id` is a decimal string (preserving 64-bit precision) and is process-local, so it may repeat after a restart. Use CloudEvents `id` (paired with `source`) to identify events uniquely across restarts.

## HMAC signing

To sign hook deliveries, set `signing_secret` on a hook using any supported [secret form](configuration.md#secrets) (inline string, `"${VAR}"`, or `{ file = "/path" }`):

```toml
[hook.automation]
url = "https://automation.example.internal/rushls"
events = ["publisher.started", "publisher.ended"]
signing_secret = { file = "/run/secrets/rushls-webhook-signing" }
```

The secret must start with `whsec_` followed by base64-encoded random bytes (at least 32 decoded bytes). Keys load at startup and are redacted from debug output and configuration errors.

Signing follows the symmetric [Standard Webhooks specification](https://github.com/standard-webhooks/standard-webhooks/blob/main/spec/standard-webhooks.md). Signed requests carry three headers:

```text
webhook-id: <CloudEvents id>
webhook-timestamp: <delivery-attempt Unix timestamp in seconds>
webhook-signature: v1,<base64 HMAC-SHA256>
```

The HMAC-SHA256 signature is computed with the decoded key (after `whsec_`) over:

```text
UTF8(webhook-id) + "." + UTF8(webhook-timestamp) + "." + raw HTTP body
```

Retries keep the original CloudEvents `id`, `time`, and raw body, updating `webhook-timestamp` and `webhook-signature` on each attempt.

To verify a webhook:

1. Read the raw HTTP body before parsing JSON.
2. Verify `webhook-signature` using a Standard Webhooks library or a constant-time HMAC comparison.
3. Reject `webhook-timestamp` values outside your replay window (typically 5 minutes).
4. Confirm `webhook-id` matches the CloudEvents `id` in the body.
5. Deduplicate verified events by `source` and `id`.

A valid signature authenticates the sender but does not stop replays on its own—always check the timestamp and deduplicate. HMAC signing works alongside bearer tokens (`token`) and mutual TLS (`client_cert`, `client_key`, `ca`). Without `signing_secret`, hooks are sent unsigned.
