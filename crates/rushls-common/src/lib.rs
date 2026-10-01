//! Infrastructure shared by Rushls and its sibling services.
//!
//! Each module sits behind a feature of the same name, so a dependent compiles
//! only what it uses. None of them choose application policy: they load
//! configuration, talk to operator-configured services, terminate TLS, and
//! accept connections, and leave reporting to the application's own events.

#[cfg(feature = "accept")]
pub mod accept;
#[cfg(feature = "config")]
pub mod config;
#[cfg(feature = "hooks")]
pub mod hooks;
#[cfg(feature = "metrics")]
pub mod metrics;
#[cfg(feature = "outbound")]
pub mod outbound;
#[cfg(feature = "proxy-protocol")]
pub mod proxy_protocol;
#[cfg(feature = "tls")]
pub mod tls;
