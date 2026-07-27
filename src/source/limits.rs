//! What one publisher is allowed to push through the front of the pipeline.
//!
//! Everything downstream reuses caller-owned buffers, which is only a bounded
//! design if something bounds how much a single call may produce. Nothing in
//! [`PacketSource`](super::PacketSource) or
//! [`MediaNormalizer`](crate::media::MediaNormalizer) can be trusted to
//! self-limit: the first is driven by a remote peer, and the second turns one
//! packet into an unbounded number of samples in principle. So the driving loop
//! measures both, at the two points where a batch materialises, and fails the
//! session rather than letting a buffer grow to whatever the input asks for.

use std::time::Duration;

use crate::domain::Appender;
use thiserror::Error;

use super::Packet;

/// Bounds on volume and rate for a single publishing session.
///
/// The batch limits protect memory: they cap how large a reused buffer can get
/// in one call, which in turn caps the capacity it retains for the rest of the
/// session. The media-density limits protect CPU and downstream bandwidth: a
/// publisher that stays under the batch caps can still send unbounded tiny
/// packets or samples without advancing its normalized timeline.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct InputLimits {
    /// Most packets one [`PacketSource::fill`](super::PacketSource::fill) may
    /// append.
    pub maximum_packets_per_batch: usize,
    /// Largest payload one demuxed packet may retain.
    pub maximum_payload_bytes_per_packet: usize,
    /// Combined packet payload retained by one source call.
    pub maximum_payload_bytes_per_batch: usize,
    /// Most samples one batch of packets may normalize into.
    ///
    /// A normalizer expands rather than contracts — it may split a packet, and
    /// it releases reordered samples in bursts — so this is deliberately looser
    /// than the packet cap rather than equal to it.
    pub maximum_samples_per_batch: usize,
    pub maximum_bytes_per_media_second: u64,
    pub maximum_packets_per_media_second: u64,
    pub maximum_samples_per_media_second: u64,
    /// Averaging window for media-density limits.
    ///
    /// Rates are measured over fixed, non-overlapping windows, so a publisher
    /// straddling a window edge can briefly sustain twice the configured rate.
    /// That slack is intentional: it keeps the check to two additions per
    /// batch, and the limits exist to stop runaway inputs, not to shape traffic
    /// precisely.
    pub media_density_window: Duration,
}

impl InputLimits {
    /// Generous headroom over any realistic contribution feed.
    ///
    /// 100 Mb/s and 50k packets/s are far above a single 4K contribution
    /// encoder, so a legitimate publisher never notices these; they exist to
    /// put a ceiling on what a hostile one can cost.
    pub fn permissive() -> Self {
        Self {
            maximum_packets_per_batch: 4_096,
            maximum_payload_bytes_per_packet: 8 * 1024 * 1024,
            maximum_payload_bytes_per_batch: 16 * 1024 * 1024,
            maximum_samples_per_batch: 16_384,
            maximum_bytes_per_media_second: 12_500_000,
            maximum_packets_per_media_second: 50_000,
            maximum_samples_per_media_second: 50_000,
            media_density_window: Duration::from_secs(1),
        }
    }
}

#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
pub enum LimitError {
    #[error("the input produced {found} {unit} in one batch, above the permitted {limit}")]
    BatchTooLarge {
        unit: BatchUnit,
        limit: usize,
        found: usize,
    },
    #[error("a packet payload was {found} bytes, above the permitted {limit}")]
    PacketPayloadTooLarge { limit: usize, found: usize },
    #[error("packet payloads totalled {found} bytes in one batch, above the permitted {limit}")]
    BatchPayloadTooLarge { limit: usize, found: usize },
    #[error("packet payload byte accounting overflowed")]
    PayloadBytesOverflow,
    #[error("the media contained {observed} {unit}/s, above the permitted {limit}/media-s")]
    MediaDensityExceeded {
        unit: DensityUnit,
        limit: u64,
        observed: u64,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PacketBatchStats {
    pub packets: usize,
    pub payload_bytes: usize,
}

/// Bounds packet count and payload memory before either is retained.
pub struct BoundedPacketBatch<'a> {
    target: &'a mut dyn Appender<Packet>,
    maximum_packets: usize,
    maximum_packet_bytes: usize,
    maximum_batch_bytes: usize,
    packets: usize,
    payload_bytes: usize,
    error: Option<LimitError>,
}

impl<'a> BoundedPacketBatch<'a> {
    pub fn new(
        target: &'a mut dyn Appender<Packet>,
        maximum_packets: usize,
        maximum_packet_bytes: usize,
        maximum_batch_bytes: usize,
    ) -> Self {
        Self {
            target,
            maximum_packets,
            maximum_packet_bytes,
            maximum_batch_bytes,
            packets: 0,
            payload_bytes: 0,
            error: None,
        }
    }

    pub fn produced(self) -> Result<PacketBatchStats, LimitError> {
        if let Some(error) = self.error {
            return Err(error);
        }
        Ok(PacketBatchStats {
            packets: self.packets,
            payload_bytes: self.payload_bytes,
        })
    }
}

impl Appender<Packet> for BoundedPacketBatch<'_> {
    fn push(&mut self, packet: Packet) {
        self.packets = self.packets.saturating_add(1);
        if self.error.is_some() {
            return;
        }
        if self.packets > self.maximum_packets {
            self.error = Some(LimitError::BatchTooLarge {
                unit: BatchUnit::Packets,
                limit: self.maximum_packets,
                found: self.packets,
            });
            return;
        }

        let packet_bytes = packet.retained_payload_bytes();
        if packet_bytes > self.maximum_packet_bytes {
            self.error = Some(LimitError::PacketPayloadTooLarge {
                limit: self.maximum_packet_bytes,
                found: packet_bytes,
            });
            return;
        }
        let Some(payload_bytes) = self.payload_bytes.checked_add(packet_bytes) else {
            self.error = Some(LimitError::PayloadBytesOverflow);
            return;
        };
        if payload_bytes > self.maximum_batch_bytes {
            self.error = Some(LimitError::BatchPayloadTooLarge {
                limit: self.maximum_batch_bytes,
                found: payload_bytes,
            });
            return;
        }

        self.payload_bytes = payload_bytes;
        self.target.push(packet);
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, derive_more::Display)]
pub enum BatchUnit {
    #[display("packets")]
    Packets,
    #[display("samples")]
    Samples,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, derive_more::Display)]
pub enum DensityUnit {
    #[display("bytes")]
    Bytes,
    #[display("packets")]
    Packets,
    #[display("samples")]
    Samples,
}

/// An [`Appender`] that stops storing once a stage overruns its batch cap.
///
/// Checking the length afterwards would be simpler, but it only detects the
/// overrun once the memory has already been committed — a stage that appended
/// ten million items would allocate for all of them before anyone objected.
/// Refusing to store past the cap bounds the damage to the cap itself.
///
/// Dropping items is safe here only because overrunning always ends the session:
/// [`Self::produced`] reports the error, the caller fails, and nothing is
/// published from a batch that was silently truncated.
pub struct BoundedBatch<'a, T> {
    target: &'a mut dyn Appender<T>,
    unit: BatchUnit,
    limit: usize,
    produced: usize,
}

impl<'a, T> BoundedBatch<'a, T> {
    pub fn new(target: &'a mut dyn Appender<T>, unit: BatchUnit, limit: usize) -> Self {
        Self {
            target,
            unit,
            limit,
            produced: 0,
        }
    }

    /// How many items this batch produced, or why it was refused.
    pub fn produced(self) -> Result<usize, LimitError> {
        if self.produced > self.limit {
            return Err(LimitError::BatchTooLarge {
                unit: self.unit,
                limit: self.limit,
                found: self.produced,
            });
        }
        Ok(self.produced)
    }
}

impl<T> Appender<T> for BoundedBatch<'_, T> {
    fn push(&mut self, item: T) {
        self.produced = self.produced.saturating_add(1);
        if self.produced > self.limit {
            return;
        }
        self.target.push(item);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn packet(bytes: usize) -> Packet {
        Packet {
            track_id: crate::domain::TrackId(0),
            pts: None,
            dts: None,
            duration: None,
            random_access: false,
            audio_trim: crate::domain::AudioTrim::default(),
            webvtt: crate::domain::WebVttCueMetadata::default(),
            subtitle_position: None,
            payload: crate::domain::Payload::from(vec![0; bytes]),
        }
    }

    #[test]
    fn packet_limits_include_promoted_subtitle_side_data() {
        let mut target = Vec::new();
        let mut batch = BoundedPacketBatch::new(&mut target, 1, 7, 7);
        let mut cue = packet(4);
        cue.webvtt.identifier = Some(std::sync::Arc::from("four"));

        batch.push(cue);

        assert_eq!(
            batch.produced(),
            Err(LimitError::PacketPayloadTooLarge { limit: 7, found: 8 })
        );
        assert!(target.is_empty());
    }

    #[test]
    fn a_batch_at_its_cap_is_stored_whole() {
        let mut target = Vec::new();
        let mut batch = BoundedBatch::new(&mut target, BatchUnit::Packets, 4);

        for item in 0..4 {
            batch.push(item);
        }

        assert_eq!(batch.produced(), Ok(4));
        assert_eq!(target, vec![0, 1, 2, 3]);
    }

    #[test]
    fn a_batch_above_its_cap_stops_allocating_and_reports_what_it_saw() {
        let mut target = Vec::new();
        let mut batch = BoundedBatch::new(&mut target, BatchUnit::Samples, 4);

        for item in 0..1_000 {
            batch.push(item);
        }

        assert_eq!(
            batch.produced(),
            Err(LimitError::BatchTooLarge {
                unit: BatchUnit::Samples,
                limit: 4,
                found: 1_000,
            })
        );
        assert_eq!(
            target.len(),
            4,
            "the overrun is bounded by the cap, not merely detected after the fact"
        );
    }

    #[test]
    fn a_bounded_batch_appends_after_what_the_buffer_already_held() {
        let mut target = vec![7, 8];
        let mut batch = BoundedBatch::new(&mut target, BatchUnit::Packets, 2);

        batch.push(9);

        assert_eq!(
            batch.produced(),
            Ok(1),
            "only this batch's items are counted"
        );
        assert_eq!(target, vec![7, 8, 9]);
    }

    #[test]
    fn an_oversized_packet_payload_is_not_retained() {
        let mut target = Vec::new();
        let mut batch = BoundedPacketBatch::new(&mut target, 4, 3, 8);

        batch.push(packet(4));

        assert_eq!(
            batch.produced(),
            Err(LimitError::PacketPayloadTooLarge { limit: 3, found: 4 })
        );
        assert!(target.is_empty());
    }

    #[test]
    fn packet_payload_accumulation_is_checked_before_retention() {
        let mut target = Vec::new();
        let mut batch = BoundedPacketBatch::new(&mut target, 4, 8, 5);

        batch.push(packet(3));
        batch.push(packet(3));

        assert_eq!(
            batch.produced(),
            Err(LimitError::BatchPayloadTooLarge { limit: 5, found: 6 })
        );
        assert_eq!(target.len(), 1);
    }
}
