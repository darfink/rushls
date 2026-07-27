use std::{collections::VecDeque, sync::Arc};

use thiserror::Error;

use crate::{
    delivery::hls::{HlsError, HlsPublisher, PublishOutcome},
    domain::{Appender, BoxFuture},
    media::{
        MediaDensityError, MediaDensityWindow, MediaError, MediaNormalizer, MediaPacer,
        NormalizedSample, PacingError, SampleSource, TimelineCalibration,
    },
    mux::{FinishReason, MuxError, Muxer, PackagedMedia},
    observe::{DeliveryMeters, MediaMeters, MuxMeters},
    source::{
        BatchUnit, BoundedBatch, BoundedPacketBatch, InputLimits, InputState, Packet, PacketSource,
    },
};

#[derive(Clone, Debug, Error, Eq, PartialEq)]
pub enum ExecutionError {
    #[error(transparent)]
    Media(#[from] MediaError),
    #[error(transparent)]
    Mux(#[from] MuxError),
    #[error(transparent)]
    Delivery(#[from] HlsError),
    #[error(transparent)]
    Pacing(#[from] PacingError),
}

/// Starting capacity for the reused buffers.
///
/// Sized for a batch of an ordinary contribution feed so steady state is
/// reached immediately, and small enough that a session which never gets there
/// has wasted nothing worth counting.
const INITIAL_BATCH: usize = 256;

/// The continuously running front of the pipeline: packets in, samples out.
///
/// Exists from the moment the presentation is calibrated until the input ends.
/// Pre-roll and the live loop both drive this same object, which is why neither
/// has to own it.
///
/// This is also where a publisher's cost is measured and capped. It is the only
/// place that sees both a whole batch of packets and the samples it expanded
/// into, so it is the only place that can tell a busy encoder from a hostile
/// one.
pub struct MediaHead {
    source: Box<dyn PacketSource>,
    normalizer: Box<dyn MediaNormalizer>,
    meters: Arc<dyn MediaMeters>,
    limits: InputLimits,
    density: MediaDensityWindow,
    packets: Vec<Packet>,
    normalized: Vec<NormalizedSample>,
    drained: bool,
}

impl MediaHead {
    pub fn new(
        source: Box<dyn PacketSource>,
        normalizer: Box<dyn MediaNormalizer>,
        meters: Arc<dyn MediaMeters>,
        limits: InputLimits,
        timeline: &TimelineCalibration,
    ) -> Self {
        Self {
            source,
            normalizer,
            meters,
            limits,
            density: MediaDensityWindow::new(limits, timeline),
            packets: Vec::with_capacity(INITIAL_BATCH.min(limits.maximum_packets_per_batch)),
            normalized: Vec::with_capacity(INITIAL_BATCH.min(limits.maximum_samples_per_batch)),
            drained: false,
        }
    }

    /// Releases access units the normalizer is still holding back.
    ///
    /// Deliberately **not** `async`: [`PacketSource`] can only be read through
    /// a future, so a synchronous flush is structurally incapable of waiting for
    /// more input. That is the guarantee the drain contract rests on — a
    /// cancelled or displaced publisher cannot keep a session alive by simply
    /// declining to send anything more.
    ///
    /// Idempotent, so the natural end-of-input path having already flushed
    /// costs nothing.
    pub fn flush(&mut self, out: &mut dyn Appender<NormalizedSample>) -> Result<(), MediaError> {
        if self.drained {
            return Ok(());
        }
        self.drained = true;
        self.normalizer.finish(out)?;
        Ok(())
    }
}

impl SampleSource for MediaHead {
    fn next_batch<'a>(
        &'a mut self,
        out: &'a mut dyn Appender<NormalizedSample>,
    ) -> BoxFuture<'a, Result<InputState, MediaError>> {
        Box::pin(async move {
            if self.drained {
                return Ok(InputState::Closed);
            }

            self.packets.clear();
            let mut packets = BoundedPacketBatch::new(
                &mut self.packets,
                self.limits.maximum_packets_per_batch,
                self.limits.maximum_payload_bytes_per_packet,
                self.limits.maximum_payload_bytes_per_batch,
            );
            let state = self.source.fill(&mut packets).await?;
            let consumed = packets.produced()?;
            self.normalized.clear();
            let mut samples = BoundedBatch::new(
                &mut self.normalized,
                BatchUnit::Samples,
                self.limits.maximum_samples_per_batch,
            );
            for packet in self.packets.drain(..) {
                self.normalizer.push(packet, &mut samples)?;
            }
            if !state.is_open() {
                // End of input is the only place trailing access units held for
                // reordering can be flushed, so it happens here rather than in
                // each of the two loops that drive this.
                self.normalizer.finish(&mut samples)?;
                self.drained = true;
            }
            let produced = samples.produced()?;
            if let Err(error) = self.density.admit(
                consumed.payload_bytes as u64,
                consumed.packets as u64,
                &self.normalized,
            ) {
                return Err(match error {
                    MediaDensityError::Limit(error) => MediaError::Limit(error),
                    other => MediaError::Density(other),
                });
            }
            for sample in self.normalized.drain(..) {
                out.push(sample);
            }

            // One atomic update per batch instead of one per packet: the loop
            // that moves the data is also the one that knows how much moved.
            self.meters
                .media_progress(consumed.packets as u64, produced as u64);
            Ok(state)
        })
    }
}

/// The swappable back of the pipeline, installed once segmentation is locked.
pub struct MediaTail {
    muxer: Box<dyn Muxer>,
    publisher: Box<dyn HlsPublisher>,
    mux_meters: Arc<dyn MuxMeters>,
    delivery_meters: Arc<dyn DeliveryMeters>,
    media: Vec<PackagedMedia>,
}

impl MediaTail {
    pub fn new(
        muxer: Box<dyn Muxer>,
        publisher: Box<dyn HlsPublisher>,
        mux_meters: Arc<dyn MuxMeters>,
        delivery_meters: Arc<dyn DeliveryMeters>,
    ) -> Self {
        Self {
            muxer,
            publisher,
            mux_meters,
            delivery_meters,
            media: Vec::with_capacity(INITIAL_BATCH),
        }
    }

    fn write_all(
        &mut self,
        samples: &mut VecDeque<NormalizedSample>,
    ) -> Result<(), ExecutionError> {
        while let Some(sample) = samples.pop_front() {
            self.muxer.push(sample, &mut self.media)?;
        }
        let (chunks, segments) = count_packaged(&self.media);
        self.mux_meters.mux_progress(chunks, segments);
        self.publish()
    }

    fn write_one(&mut self, sample: NormalizedSample) -> Result<(), ExecutionError> {
        self.muxer.push(sample, &mut self.media)?;
        let (chunks, segments) = count_packaged(&self.media);
        self.mux_meters.mux_progress(chunks, segments);
        self.publish()
    }

    fn finish(&mut self, reason: FinishReason) -> Result<(), ExecutionError> {
        self.muxer.finish(reason, &mut self.media)?;
        let (chunks, segments) = count_packaged(&self.media);
        self.mux_meters.mux_progress(chunks, segments);
        self.publish()?;
        self.publisher.finish(reason)?;
        Ok(())
    }

    fn publish(&mut self) -> Result<(), ExecutionError> {
        let mut parts = 0;
        let mut segments = 0;
        let mut result = Ok(());

        for media in self.media.drain(..) {
            let (chunk, completed_segment) = media_object_count(&media);
            match self.publisher.write(media) {
                Ok(PublishOutcome::Published) => {
                    // Packaging calls these chunks; HLS projects each one as
                    // a partial segment and therefore reports it as a part.
                    parts += chunk;
                    segments += completed_segment;
                }
                Ok(PublishOutcome::Superseded) => {}
                Err(error) => {
                    result = Err(error.into());
                    break;
                }
            }
        }
        self.media.clear();

        // Reported even when publication failed part way, so the counters
        // reflect what viewers actually received.
        self.delivery_meters.delivery_progress(parts, segments);
        result
    }
}

fn count_packaged(media: &[PackagedMedia]) -> (u64, u64) {
    media.iter().fold((0, 0), |(chunks, segments), item| {
        let (chunk, segment) = media_object_count(item);
        (chunks + chunk, segments + segment)
    })
}

/// Returns the `(chunks, completed segments)` represented by one event.
///
/// Initialization events produce no media object. A direct segment and a
/// chunked segment completion each represent one completed segment, while the
/// completion carries no second copy of its payload.
fn media_object_count(media: &PackagedMedia) -> (u64, u64) {
    match media {
        PackagedMedia::Initialization(_) => (0, 0),
        PackagedMedia::Chunk(_) => (1, 0),
        PackagedMedia::Segment(_) | PackagedMedia::SegmentCompleted(_) => (0, 1),
    }
}

/// The assembled pipeline, from input through to publication.
pub struct LiveSession {
    head: MediaHead,
    tail: MediaTail,
    pacer: MediaPacer,
    samples: VecDeque<NormalizedSample>,
    input_state: InputState,
    replayed: bool,
    drained: bool,
}

impl LiveSession {
    /// Assembles the pipeline, taking ownership of the samples pre-roll
    /// buffered while it discovered where segments begin.
    pub fn new(
        head: MediaHead,
        tail: MediaTail,
        pacer: MediaPacer,
        buffered: Vec<NormalizedSample>,
        input_state: InputState,
    ) -> Self {
        Self {
            head,
            tail,
            pacer,
            samples: buffered.into(),
            input_state,
            replayed: false,
            drained: false,
        }
    }

    /// Advances the pipeline by one batch, preserving how input ended.
    ///
    /// Stepping a batch at a time is what lets supervision interleave stop
    /// checks and health evaluation without ever cancelling a partially
    /// processed read.
    pub async fn pump(&mut self) -> Result<InputState, ExecutionError> {
        if !self.replayed {
            self.replayed = true;
            self.tail.write_all(&mut self.samples)?;
            self.drained = !self.input_state.is_open();
            return Ok(self.input_state);
        }
        if self.drained {
            return Ok(InputState::Closed);
        }

        if self.samples.is_empty() {
            self.input_state = self.head.next_batch(&mut self.samples).await?;
        }

        // Leave each accepted sample queued until its delay has completed.
        // This makes pacing cancellation-safe: a supervision tick or stop can
        // cancel this future without losing the batch or accidentally treating
        // its next sample as already paced.
        while let Some(sample) = self.samples.front() {
            self.pacer.pace(sample).await?;
            let sample = self
                .samples
                .pop_front()
                .expect("the paced front sample is still queued");
            self.tail.write_one(sample)?;
        }
        self.drained = !self.input_state.is_open();
        Ok(self.input_state)
    }

    /// Flushes media already accepted, then closes out the publication.
    ///
    /// Runs whether the session ended naturally or was stopped, so a cancelled
    /// publisher still leaves a playable stream behind.
    ///
    /// # Drain contract
    ///
    /// This is a bounded flush of media the pipeline has *already accepted*, and
    /// nothing more:
    ///
    /// - **No new input is consumed.** Every step here is synchronous, and the
    ///   only way to read a [`PacketSource`] is to await it, so this cannot wait
    ///   on a publisher that has stopped sending — the guarantee is enforced by
    ///   the signatures, not by convention.
    /// - **No wall-clock deadline is needed**, because the work is finite before
    ///   it starts: the normalizer's reorder buffer, the muxer's open part and
    ///   segment, and whatever is left in this session's own buffers. All three
    ///   are bounded by [`InputLimits`], which no batch was allowed to exceed.
    /// - **A final partial segment may be emitted under
    ///   [`FinishReason::Final`] or [`FinishReason::Interrupted`].** Both lack a
    ///   known successor, so retaining accepted media is preferable. Under
    ///   [`FinishReason::Superseded`] the successor's media follows immediately,
    ///   so a stunted trailing segment is worse than none.
    /// - **A muxer that cannot finalize does not lose the flush.** The
    ///   normalizer is flushed first and its samples written before the muxer is
    ///   closed, so a failure at the last step still leaves everything that had
    ///   already been segmented published. The error is reported, not swallowed,
    ///   but it does not turn a completed session into a failed one.
    /// - **The successor owns the visible stream from the instant it claims
    ///   it.** A displaced publisher's lease is revoked when the takeover leases
    ///   the stream, so anything this produces afterwards is discarded rather
    ///   than interleaved. There is no overlap in which ownership is ambiguous.
    pub fn finish(&mut self, reason: FinishReason) -> Result<(), ExecutionError> {
        // Flush before closing the muxer: samples held back for reordering are
        // media the publisher already sent and we already accepted, and a
        // cancellation is no reason to drop them on the floor.
        self.head.flush(&mut self.samples)?;
        self.drained = true;
        self.tail.write_all(&mut self.samples)?;
        self.tail.finish(reason)
    }
}

#[cfg(test)]
mod tests {
    use crate::{
        delivery::hls::PublishOutcome,
        domain::{Appender, Payload},
        mux::{Muxer, PackagedChunk, PackagingRenditionId, PackagingSegmentId},
        observe::{ProcessMeters, SessionMeters},
    };

    use super::*;

    struct IdleMuxer;

    impl Muxer for IdleMuxer {
        fn expected_publication_interval(&self) -> std::time::Duration {
            std::time::Duration::from_secs(1)
        }

        fn push(
            &mut self,
            _sample: NormalizedSample,
            _out: &mut dyn Appender<PackagedMedia>,
        ) -> Result<(), MuxError> {
            Ok(())
        }

        fn finish(
            &mut self,
            _reason: FinishReason,
            _out: &mut dyn Appender<PackagedMedia>,
        ) -> Result<(), MuxError> {
            Ok(())
        }
    }

    struct RevokedPublisher;

    impl HlsPublisher for RevokedPublisher {
        fn write(&mut self, _media: PackagedMedia) -> Result<PublishOutcome, HlsError> {
            Ok(PublishOutcome::Superseded)
        }

        fn finish(&mut self, _reason: FinishReason) -> Result<(), HlsError> {
            Ok(())
        }
    }

    #[test]
    fn a_revoked_write_does_not_advance_delivery_meters() {
        let process = ProcessMeters::default();
        let session = SessionMeters::new(process.clone());
        let mut tail = MediaTail::new(
            Box::new(IdleMuxer),
            Box::new(RevokedPublisher),
            session.mux_view(),
            session.delivery_view(),
        );
        tail.media.push(PackagedMedia::Chunk(PackagedChunk {
            rendition_id: PackagingRenditionId(0),
            packaging_segment_id: PackagingSegmentId(0),
            chunk_index: 0,
            media_start: 0,
            duration: 1,
            independent: true,
            payload: Payload::from(vec![1]),
        }));

        tail.publish()
            .expect("revocation is not a publication error");

        assert_eq!(process.snapshot().parts_published, 0);
    }
}
