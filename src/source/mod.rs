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

pub use byte::{ByteInput, ByteInputError, ReadInput};

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

/// Records the first presentation timestamp, excluding declared encoder priming.
fn record_first_pts(
    track: &mut crate::domain::DiscoveredTrack,
    packet: &Packet,
) -> Result<(), SourceError> {
    if track.first_pts.is_some() {
        return Ok(());
    }
    let Some(pts) = packet.pts.or(packet.dts) else {
        return Ok(());
    };
    let padding = if let crate::domain::MediaParameters::Audio {
        timing,
        sample_rate,
        ..
    } = track.parameters
    {
        let numerator =
            u64::from(timing.initial_padding_samples) * u64::from(track.timebase.den().get());
        let denominator = u64::from(sample_rate.get()) * u64::from(track.timebase.num().get());
        if !numerator.is_multiple_of(denominator) {
            return Err(SourceError::Demux(
                "audio priming is inexact in the source timebase".into(),
            ));
        }
        numerator / denominator
    } else {
        0
    };
    track.first_pts = Some(
        pts.checked_add_unsigned(padding)
            .ok_or_else(|| SourceError::Demux("first presentation timestamp overflowed".into()))?,
    );
    Ok(())
}
