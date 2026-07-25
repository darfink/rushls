use std::time::Duration;

use thiserror::Error;

use crate::domain::{Appender, BoxFuture, Payload, TrackCatalog, TrackId};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DiscoveryLimits {
    pub maximum_probe_bytes: usize,
    pub maximum_wall_time: Duration,
}

/// One demultiplexed access unit, still in its declared timebase.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Packet {
    pub track_id: TrackId,
    pub pts: Option<i64>,
    pub dts: Option<i64>,
    pub duration: Option<i64>,
    pub random_access: bool,
    pub payload: Payload,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DiscoveryReport {
    pub tracks: TrackCatalog,
}

/// Whether more media is coming, and if not, whether the publisher meant it.
///
/// The distinction is the transport's to make and nobody else's: only the
/// adapter sees the difference between an RTMP `FCUnpublish` and a TCP reset.
/// It matters because it decides what viewers are told. Reporting a dropped
/// contribution link as an end of stream tears down every player watching,
/// seconds before the encoder reconnects — which is exactly the outage the
/// store's reconnect budget exists to hide.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum InputState {
    Open,
    /// The publisher said it was done.
    Closed,
    /// The input stopped without saying anything. The same publisher may come
    /// back for this stream.
    Interrupted,
}

impl InputState {
    pub fn is_open(self) -> bool {
        matches!(self, Self::Open)
    }
}

#[derive(Clone, Debug, Error, Eq, PartialEq)]
pub enum SourceError {
    #[error("failed to open input: {0}")]
    Open(String),
    #[error("stream discovery failed: {0}")]
    Discovery(String),
    #[error("the input introduced a new track after discovery")]
    TrackSetChanged,
    #[error("codec parameters changed after discovery for {track_id}")]
    CodecParametersChanged { track_id: TrackId },
    #[error("packet payload was {found} bytes, above the permitted {limit}")]
    PacketPayloadTooLarge { limit: usize, found: usize },
    #[error("input failed: {0}")]
    Input(String),
}

/// A running, demultiplexed input.
///
/// [`Self::fill`] is batch-oriented so the whole pipeline needs one `await` per
/// socket read rather than one per packet. That keeps the boxed future off the
/// per-packet path, and it gives the runtime a natural place to hand muxing to
/// a blocking thread later without reshaping any caller.
///
/// Output goes to an [`Appender`], so the caller's buffer can only grow. See
/// that trait for why. Appending some packets and then failing is allowed — the
/// caller discards a failed batch wholesale — so an implementation never has to
/// unwind partial output.
pub trait PacketSource: Send {
    /// Discovers stream metadata.
    ///
    /// Packets consumed while probing must remain available, in order, through
    /// [`Self::fill`]; discovery observes the input, it does not consume it.
    ///
    /// `limits` is advisory here — the caller also enforces
    /// [`DiscoveryLimits::maximum_wall_time`] from the outside, since an
    /// implementation that hangs cannot be trusted to time itself out.
    fn discover<'a>(
        &'a mut self,
        limits: DiscoveryLimits,
    ) -> BoxFuture<'a, Result<DiscoveryReport, SourceError>>;

    /// Appends the next batch of packets to `out`.
    ///
    /// Reports the input exhausted only after appending whatever remained, and
    /// says why — see [`InputState`]. An implementation that cannot tell a
    /// deliberate close from a lost connection should report
    /// [`InputState::Interrupted`], which is the answer that costs viewers
    /// least if it turns out to be wrong.
    ///
    /// Implementations should report volume to their
    /// [`SourceMeters`](crate::observe::SourceMeters) once per call rather than
    /// once per packet.
    ///
    /// How much one call may append is capped by
    /// [`InputLimits::maximum_packets_per_batch`](super::InputLimits::maximum_packets_per_batch),
    /// [`InputLimits::maximum_payload_bytes_per_packet`](super::InputLimits::maximum_payload_bytes_per_packet),
    /// and [`InputLimits::maximum_payload_bytes_per_batch`](super::InputLimits::maximum_payload_bytes_per_batch).
    /// The caller refuses excess output, but concrete sources should apply the
    /// byte limits before allocating payload storage. An implementation should
    /// stop at a natural boundary well below the caps; exceeding one ends the
    /// session.
    fn fill<'a>(
        &'a mut self,
        out: &'a mut dyn Appender<Packet>,
    ) -> BoxFuture<'a, Result<InputState, SourceError>>;
}
