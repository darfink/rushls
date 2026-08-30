//! Requests this node makes to services an administrator configured.
//!
//! The implementation moved to `cc-outbound` so the RTMP proxy shares it: both
//! call operator-supplied endpoints and need the same properties - a pooled
//! connection, bounded time, a bounded response, and a bearer credential.
//!
//! Re-exported under the original path rather than rewritten at every call
//! site, so the move is invisible to the rest of this crate and the shared
//! crate stays free to grow its own module layout.

pub use cc_outbound::*;
