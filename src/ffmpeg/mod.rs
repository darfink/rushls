//! Crate-private FFmpeg ownership and conversion utilities.
//!
//! Both AVFormat ingest and CMAF output use these wrappers. Keeping them below
//! the source and mux facades prevents raw FFmpeg types and lifetime rules from
//! becoming part of either public contract.

mod dictionary;
mod error;
mod packet;
mod rational;

pub use dictionary::{Dictionary, value};
pub use error::AvError;
pub use packet::OwnedPacket;
pub use rational::{from_av_rational, to_av_rational, RationalError};
