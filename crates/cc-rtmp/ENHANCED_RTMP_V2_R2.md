# Enhanced RTMP v2 r2 compliance

This matrix is pinned to VSO Enhanced RTMP document
`v2-2026-01-31-r2` at commit
`9f371058b9c24e269cdadc34295333793b326719`:

<https://github.com/veovera/enhanced-rtmp/blob/9f371058b9c24e269cdadc34295333793b326719/docs/enhanced/enhanced-rtmp-v2.md>

`cc-rtmp` uses `scuffle-flv` 0.2.2 for typed demuxing, then applies the
v2 r2 deltas and validation policy in its owned API. Raw RTMP message bodies
remain authoritative.

| v2 r2 area | Ingest and inspection | Lossless relay | Notes |
|---|---:|---:|---|
| Legacy audio/video FLV | Yes | Yes | Parsed by `scuffle-flv`; raw bytes retained. |
| Enhanced video packet families | Yes | Yes | Sequence start/end, coded frames, coded frames X, metadata, MPEG-2 TS, multitrack, and ModEx. |
| Enhanced audio packet families | Yes | Yes | Sequence start/end, coded frames, multichannel configuration, multitrack, and ModEx. |
| Video FourCCs | Yes | Yes | VP8, VP9, AV1, AVC, HEVC, and v2 r2 VVC (`vvc1`). VVC is represented by the extensible FourCC type because `scuffle-flv` predates its named constant. |
| Audio FourCCs | Yes | Yes | AC-3, E-AC-3, Opus, MP3, FLAC, and AAC. |
| Multitrack IDs and per-track codecs | Yes | Yes | Track IDs are exposed; no priority is inferred from the ID. Track metadata remains in the raw script-data payload. |
| Multichannel audio | Yes | Yes | Unspecified, native mask, and custom channel order are parsed. |
| ModEx timestamp nano offset | Yes | Yes | Known ModEx is typed. Unknown ModEx is rejected in strict mode and opaque in passthrough mode. |
| Video metadata frames | Yes | Yes | Typed metadata entries plus original bytes. |
| `onMetaData` and track maps | Yes | Yes | Additional audio/video track IDs, codec identifiers, and arbitrary per-track fields have an owned typed view. All encoded AMF bytes remain authoritative and are relayed byte-for-byte. |
| `fourCcList` | Yes | Yes | Strict mode requires a strict array of four-byte strings or the `*` wildcard. |
| `[audio|video]FourCcInfoMap` | Yes | Yes | Strict mode requires FourCC or wildcard keys and numeric capability masks. |
| `capsEx` | Yes | Yes | Numeric flags are retained. Servers advertise only capabilities they implement. |
| Server capability response | Yes | N/A | Server `_result` properties are configurable; clients receive the complete command and status property maps. |
| Unknown connect properties | Yes | Selected | Retained for inspection. Only E-RTMP capability fields are forwarded to a new connection; `tcUrl`, `flashVer`, and other connection-local values are not replayed. |
| Unknown or malformed Enhanced packets | Policy-dependent | Yes in passthrough | Strict rejects invalid known structures and unknown enum/FourCC values. Passthrough retains them as opaque raw bytes. |
| AMF3 script data | Raw only | Raw only | RML accepts historical type-15 format-selector behaviour, but this change does not add a full AMF3 value model. |
| Reconnect request | Parsed capability only | N/A | Neither app issues reconnect requests; they do not advertise the reconnect capability bit. |
| Typed outbound Enhanced FLV generation | No | Raw only | Deliberately deferred. Proxy forwarding uses original bytes. |
| Playback sequence caching | No | N/A | Deliberately deferred. Existing RML play APIs remain available internally. |

Application codec admission is separate from protocol validation. Rushls can,
for example, parse a structurally valid codec that its media policy later
rejects.
