//! Where media enters the node.
//!
//! Transport and demux are exposed as one accepted source, but remain separate
//! concerns inside a protocol adapter. RTMP supplies an FLV byte stream; SRT is
//! container-agnostic and may carry MPEG-TS, Matroska, or any other format the
//! configured demuxer can probe. A [`PendingPublish`] therefore wires its byte
//! transport into the demuxer and yields a ready [`PacketSource`], so session
//! orchestration cannot accidentally mismatch those implementation pieces.

mod caption;
mod limits;
mod memory;
mod packet;
mod publish;

pub mod avformat;
pub mod transport;

pub use caption::{CaptionObservation, H264CaptionDetector};
pub use memory::PipelineMemory;
pub use limits::{
    BatchUnit, BoundedBatch, BoundedPacketBatch, DensityUnit, InputLimits, LimitError,
    PacketBatchStats,
};
pub use packet::{
    DiscoveryLimits, DiscoveryProblem, DiscoveryReport, InputState, Packet, PacketSource,
    SourceError,
};
pub use publish::{AcceptedPublish, PendingPublish, PublishRejection, TransportError};
