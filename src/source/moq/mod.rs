//! Hang ingest: one moq-lite broadcast becomes one [`PacketSource`].
//!
//! Tracks are renditions. The catalog freezes at discovery; later mutation is
//! a different publication. Both LOC and legacy Hang frames are accepted.
//!
//! The wire is read directly on [`moq_net`]. Catalog JSON and frame headers
//! stay local; existing codec helpers handle H.264 and audio configuration.

pub mod catalog;
mod h264;
pub mod identity;
mod loc;
mod map;
mod source;

pub use identity::from_webtransport_url;
pub use source::MoqPacketSource;

/// Why reading a moq-lite track stopped.
///
/// The transport error is kept whole rather than flattened into a message: a
/// cancelled subscription and a corrupt frame both end a read, but only one of
/// them is a routine end of publication, and [`source`] needs to tell them
/// apart to decide whether the input closed or was interrupted.
#[derive(Debug, thiserror::Error)]
pub enum ReadError {
    #[error(transparent)]
    Moq(#[from] moq_net::Error),
    #[error("{0}")]
    Malformed(Box<str>),
}

#[cfg(test)]
mod fixtures;
