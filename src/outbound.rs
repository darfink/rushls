//! Requests this node makes to services an administrator configured.
//!
//! `rushls_common::outbound` provides pooled connections, request deadlines, response
//! limits, and bearer credentials. This module keeps transport details behind
//! one application boundary while the infrastructure crate stays independent
//! of stream identities and lifecycle events.

pub use rushls_common::outbound::*;
