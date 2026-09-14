//! Making muxed media fetchable.
//!
//! Delivery owns retained media and the read-side origin presented to protocol
//! adapters. It consumes [`PackagedMedia`](crate::mux::PackagedMedia) and knows
//! nothing about how those bytes were produced or which manifest protocol will
//! describe them.

pub mod body;
pub mod hls;
pub mod memory;
pub mod origin;
pub mod record;
pub mod response;
pub mod store;
pub mod uri;

pub use body::{MediaBody, MediaFrameIter};
pub use origin::{EdgeCondition, MediaObject, Origin, WaitOutcome};
pub use response::{Body, DeliveryError, DeliveryFailure, Response, Reuse};
