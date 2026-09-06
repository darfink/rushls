//! Where media enters the node.
//!
//! Transport and demux are exposed as one accepted source, but remain separate
//! concerns inside a protocol adapter. RTMP maps `cc-rtmp` tags through
//! [`rtmp`]; SRT carries MPEG-TS into [`mpegts`]. A [`PendingPublish`] therefore
//! wires its byte transport into the demuxer and yields a ready
//! [`PacketSource`], so session orchestration cannot accidentally mismatch
//! those implementation pieces.

mod byte;
mod caption;
#[cfg(test)]
pub(crate) mod fixtures;
mod limits;
mod memory;
mod mpegts;
mod packet;
mod publish;
mod rtmp;

pub mod transport;

pub use byte::{ByteInput, ByteInputError, ByteInterrupt, ReadInput};

pub use caption::{CaptionObservation, H264CaptionDetector};
pub use limits::{
    BatchUnit, BoundedBatch, BoundedPacketBatch, DensityUnit, InputLimits, LimitError,
    PacketBatchStats,
};
pub use memory::PipelineMemory;
pub use mpegts::{MpegTsConfig, MpegTsPacketSource};
pub use packet::{
    DiscoveryLimits, DiscoveryProblem, DiscoveryReport, InputState, Packet, PacketSource,
    SourceError,
};
pub use publish::{AcceptedPublish, PendingPublish, PublishRejection, TransportError};
#[cfg(test)]
pub(crate) use rtmp::encode_cue;
pub use rtmp::{IngressEvent, IngressWriter, RtmpPacketSource, channel};
