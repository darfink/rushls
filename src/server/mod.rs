//! Process wiring: configuration, listeners, and the operator surface.
//!
//! Everything below this layer is a library. This is the only place that knows
//! which concrete transports, muxers, and publishers a node is built from.

pub mod metrics;
