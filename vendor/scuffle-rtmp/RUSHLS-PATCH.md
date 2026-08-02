# RushLS patch

This directory vendors `scuffle-rtmp` 0.2.3 from crates.io.

The released crate and the [current upstream implementation][reader] reuse the
preceding absolute timestamp when a type-3 RTMP chunk starts a new message.
RTMP requires that message to advance by the preceding timestamp delta.
Continuation chunks of a partial message must instead retain the current
message timestamp.

RushLS records the last delta per chunk stream and distinguishes new messages
from continuations using the reader's existing partial-message state. Timestamp
addition also uses the RTMP clock's wrapping 32-bit arithmetic.

The vendored server also advertises FFmpeg-compatible 2,500,000-byte
acknowledgement and peer-bandwidth windows. Acknowledgements include the read
that crosses the window boundary and retain a separate unacknowledged-byte
counter so cadence remains correct when the 32-bit sequence number wraps. A
zero acknowledgement window from a peer is rejected.

Remove the `[patch.crates-io]` entry and this directory after an upstream
release contains the equivalent fix.

[reader]: https://github.com/ScuffleCloud/scuffle/blob/main/crates/rtmp/src/chunk/reader.rs
