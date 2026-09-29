# Publisher auth and lifecycle API

The auth request and session hooks share one transport identity shape.
`protocol`, `resource`, and `client` describe the observed publisher.
`stream_id` and `principal` describe the authorization decision.

## Admission

Rushls sends one POST with `Content-Type: application/json`:

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

`protocol` is `rtmp`, `srt`, or `moq`. `remote_address` is the transport peer's
socket address, including its port. It does not claim to identify a publisher
behind a relay. Optional transport metadata uses `null` when unavailable.
`resource.namespace` can also be `null`.

`request_id` identifies this admission attempt. It is separate from a lifecycle
`session_id`, which exists only after admission and registration. The response
schema follows the request's `version`. An allowing response is:

```json
{
  "decision": "allow",
  "stream_id": "events/main",
  "principal": "account-42",
  "profile": "premium"
}
```

`stream_id` and `principal` are required, nonblank strings. `profile` is optional
and selects a `[publish.profile.NAME]` table. Omission or `null` selects the
default `[publish]` profile.
A denying response is:

```json
{"decision": "deny", "reason": "subscription_inactive"}
```

`reason` is optional human-readable text. The node does not relay it to the
publisher. Unknown fields are refused in both response forms. The response
cannot override the observed protocol, resource, or client details.

Only successful HTTP responses contain decisions. Transport errors, other
statuses, invalid JSON, unknown policies, and invalid identities fail closed.
Rushls does not retry the request. A reconnect creates a new admission attempt.

## Lifecycle hooks

Hooks use `application/cloudevents+json`. Subscriptions use these names:

- `session.started`
- `session.ended`
- `stream.available`
- `stream.unavailable`

The wire type includes a schema version (`rushls.<kind>.v1`). New fields, such as
the `protocol`, `resource`, and `client` details on session events, are additive
and keep the version: consumers must accept and ignore fields they do not
recognize. Only a breaking change, removing or renaming a field or changing its
type or meaning, moves the type to `.v2`. Subscriptions name the kind
(`session.started`), so one subscription receives every version of that event.

```json
{
  "specversion": "1.0",
  "id": "0198abc0-0000-7000-8000-000000000002",
  "source": "origin-1",
  "type": "rushls.session.started.v1",
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

`session.ended` repeats those identity fields and adds:

```json
{
  "outcome": "ended",
  "duration_ms": 180000,
  "was_available": true,
  "diagnostic": null
}
```

`outcome` is `ended`, `interrupted`, `replaced`, `cancelled`, `unhealthy`, or
`failed`. `diagnostic` is optional human-readable detail, not a stable code.
The existing `was_available` field means this session reached its running
pipeline state. Stream playability belongs to `stream.available` instead.

Stream events carry `stream_id` in `data`, and `stream.available` adds
`playlist_path`, the multivariant playlist's server path such as
`/live/camera/index.m3u8`. Like `segment.ready` paths, it is rooted rather than
absolute: join it to the origin or CDN the consumer uses. Stream events do not
name a session, because a stream can remain playable across publisher
reconnects. Credentials
never appear in lifecycle events. Session IDs are decimal strings to preserve
64-bit precision. They are process-local and can repeat after a restart.
CloudEvents IDs identify occurrences across restarts.

## HMAC signing

Each hook can configure `signing_secret` as a string, `"${VAR}"`, or `{ file = "/path" }`.
The value is `whsec_` followed by base64 encoding of at least 32 random bytes.
Mounted secret files use the existing text-secret rules. Keys load at startup
and remain redacted in debug output and configuration errors.

```toml
[hook.automation]
url = "https://automation.example.internal/rushls"
events = ["session.started", "session.ended"]
signing_secret = { file = "/run/secrets/rushls-webhook-signing" }
```

Signing follows the symmetric
[Standard Webhooks specification](https://github.com/standard-webhooks/standard-webhooks/blob/main/spec/standard-webhooks.md).
Each signed request includes:

```text
webhook-id: <CloudEvents id>
webhook-timestamp: <delivery-attempt Unix timestamp in seconds>
webhook-signature: v1,<base64 HMAC-SHA256>
```

The signed bytes are:

```text
UTF8(webhook-id) + "." + UTF8(webhook-timestamp) + "." + raw HTTP body
```

The key is the decoded secret after the `whsec_` prefix. Retries preserve the
CloudEvents ID and body bytes. Each attempt gets a current delivery timestamp
and a signature. The CloudEvents `time` remains the occurrence time.

To verify a request:

1. Read the raw body before parsing JSON.
2. Verify the HMAC with a Standard Webhooks verifier or a constant-time comparison.
3. Reject delivery timestamps outside your replay window, typically five minutes.
4. Verify that `webhook-id` matches the signed CloudEvents `id`.
5. Deduplicate verified events by `source` and `id`.

A valid signature authenticates the request but does not prevent replay by
itself. Timestamp validation and deduplication are also required. Signing can
coexist with bearer authentication and mutual TLS. Without a signing key,
hooks preserve their existing unsigned behavior.
