# Vendored `rml_rtmp`

| | |
|---|---|
| Upstream | https://github.com/KallDrexx/rust-media-libs |
| Source | upstream `master` commit `953fc41d` (2023-05-31) |
| Previous base | crates.io `rml_rtmp` 0.8.0 |
| License | MIT (retained, see `LICENSE-MIT`) |

## Why this is vendored

`rml_rtmp` predates Enhanced RTMP (E-RTMP). Two of its APIs discard data that a
**proxy** must preserve but a standalone server can safely ignore:

1. `ClientSession::request_connection` builds a fixed `connect` command object
   (`app`, `flashVer`, `objectEncoding`, `tcUrl`) with no way to add fields. A
   proxy needs to forward the publisher's E-RTMP capability advertisement
   (`fourCcList`, `capsEx`, the FourCC info maps) to the backend, otherwise an
   OBS 30+ publisher negotiating HEVC/AV1 silently falls back to H.264.
2. `ServerSessionEvent::StreamMetadataChanged` exposes only the typed
   `StreamMetadata` fields the crate knows about, dropping every other
   `@setDataFrame` key. A proxy should relay metadata as the publisher sent it.

The fork is isolated behind `crates/cc-rtmp`; applications must not depend on it
directly.

## Local changes

Substantive local changes carry `CROWDCAST:` comments where the marker does not
obscure the surrounding code. This document is the authoritative patch
inventory; `rg 'CROWDCAST:' src/` is a useful cross-check, not a complete list.

- `Cargo.toml` - `rml_amf0` switched from a workspace path dep to the crates.io
  release, and edition/rust-version pinned to match the workspace.
- `src/sessions/client/mod.rs` - added
  `request_connection_with_properties`, which accepts extra AMF0 properties to
  merge into the `connect` command object. `request_connection` is retained and
  delegates to it, so upstream behaviour is unchanged.
- `src/sessions/server/mod.rs` - added a `raw_metadata` field carrying the
  original AMF0 values and encoded payload alongside the parsed
  `StreamMetadata`. The client can publish that encoded payload without
  decode/re-encode loss.
- `src/sessions/server/events.rs` - `ConnectionRequested` now carries
  `additional_properties`, the connect command object fields the session does
  not consume itself. Without this the E-RTMP advertisement never reaches the
  caller, because the server discarded everything except `app`.
- `src/sessions/{server,client}/mod.rs` - **acknowledgement sequence number is
  now cumulative**. The RTMP spec defines it as the total bytes received on the
  connection; upstream sent bytes-since-the-last-ack, which reports a stalled
  counter to the peer. A strict sender tracking its unacknowledged window can
  throttle or drop such a connection.
- `src/sessions/{server,client}/mod.rs` - **acknowledge on our own advertised
  window when the peer never advertises one.** Upstream only acknowledged after
  receiving a peer `WindowAcknowledgement`. ffmpeg never sends one when
  publishing - verified against ffmpeg 9.0.1, which sends only `SetChunkSize` -
  so an ffmpeg publisher was never acknowledged at all, for the whole session.
  We now fall back to the window we advertised, which is what we asked the peer
  to respect in the first place.
- Acknowledgement overshoot is retained modulo the window, zero windows are
  rejected, and byte/timestamp counters use wrapping arithmetic where RTMP
  defines rollover.
- `src/chunk_io/deserializer.rs` bounds inbound chunk size, message size,
  tracked chunk streams, concurrent partial messages, and total buffered bytes.
  `Abort` now releases the target partial message.
- `src/chunk_io/deserializer.rs` - **fixed a remotely reachable panic and
  stream corruption with interleaved chunk streams.** Upstream kept one
  `current_payload_data` buffer shared by every chunk stream id, so a message
  split across chunks was corrupted by any message interleaved on another csid -
  which RTMP explicitly permits. Partial payloads are now kept per csid.
  Separately, a peer could declare a message length shorter than the payload it
  had already sent, underflowing an unchecked subtraction and panicking the
  process. That is now the `MessageLengthSmallerThanBufferedPayload` error.
- `src/chunk_io/deserialization_errors.rs` - added that error variant.
- `src/lib.rs` - `#![allow(clippy::all)]`, so upstream's pre-existing lints do
  not mask warnings in our own code.
- `src/sessions/server/mod.rs` - **`reject_request` now answers NetStream
  requests with `onStatus`, not `_error`.** `connect` is a NetConnection
  transaction, so refusing it with `_error` and its transaction id is correct.
  `publish` and `play` are not: servers answer those with an `onStatus` event at
  `level: "error"`, which is the same command the accept paths already use for
  `NetStream.Publish.Start`. Upstream sent `_error` for all three, so an encoder
  watching for `NetStream.Publish.*` on `onStatus` never saw the rejection.
- Server control messages are retained until `connect` is accepted, so session
  construction produces no network writes. Connection acceptance can include
  E-RTMP capability properties in `_result`.
- Client connection events expose the server's complete result property maps.
- Publish mode matching includes upstream master commit `953fc41d`'s
  case-insensitive compatibility fix.
- `deleteStream` accepts GStreamer's decimal string stream IDs in addition to
  the numeric form required by the base protocol implementation.
- `rustfmt.toml` - pins upstream's formatting. The workspace uses 2-space
  indent, which would otherwise reformat every file and bury the real changes.

## Rebasing onto a new upstream

```bash
rg 'CROWDCAST:' src/          # find every local change
```

Upstream has been quiet since 2023, so the practical expectation is that this
fork is long-lived. Keep the diff minimal and additive.

The deserializer and acknowledgement fixes are genuine upstream bugs rather than
proxy-specific needs, and are worth offering back to
`KallDrexx/rust-media-libs` if the project becomes active again.
