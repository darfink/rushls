//! Process wiring: configuration, listeners, and the operator surface.
//!
//! Everything below this layer is a library. This is the only place that knows
//! which concrete transports, muxers, and publishers a node is built from.

pub mod config;
pub mod http;
pub mod metrics;
mod runtime;

pub use config::{AppConfig, ConfigError, ResolvedAppConfig};
pub use http::{AllowedOrigins, CorsConfig, TlsError, TlsSettings};
pub use runtime::{Node, NodeConfig, RuntimeError};
