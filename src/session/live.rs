use std::{collections::VecDeque, sync::Arc};

use thiserror::Error;

use crate::{
    delivery::hls::{HlsError, HlsPublisher, PublishOutcome},
    domain::{Appender, BoxFuture},
    media::{
        CaptionReconciliation, CaptionVerifier, MediaDensityWindow, MediaError, MediaNormalizer,
        MediaPacer, NormalizedSample, PacingError, SampleSource, TimelineCalibration,
    },
    mux::{CaptionChannel, ClosedCaptionService, FinishReason, MuxError, Muxer, PackagedMedia},
    observe::{DeliveryMeters, EventSink, MediaMeters, MuxMeters, SessionEvent},
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
    /// Watches video access units for in-band captions, when any track can be
    /// inspected. Absent for a publication with nothing to scan.
    captions: Option<CaptionVerifier>,
    /// Caption services observed but not yet handed to the publisher.
    ///
    /// Queued rather than declared inline because this half of the pipeline
    /// holds no publisher: the tail owns publication, so the transition is
    /// carried across in the same batch that produced it.
    declared_captions: Option<Arc<[ClosedCaptionService]>>,
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
        captions: Option<CaptionVerifier>,
    ) -> Self {
        Self {
            source,
            normalizer,
            meters,
            limits,
            density: MediaDensityWindow::new(limits, timeline),
            captions,
            declared_captions: None,
            packets: Vec::with_capacity(INITIAL_BATCH.min(limits.maximum_packets_per_batch)),
            normalized: Vec::with_capacity(INITIAL_BATCH.min(limits.maximum_samples_per_batch)),
            drained: false,
        }
    }

    /// Takes any caption declaration produced since this was last called.
    pub fn take_caption_declaration(&mut self) -> Option<Arc<[ClosedCaptionService]>> {
        self.declared_captions.take()
    }

    /// How the observed captions currently reconcile across video tracks.
    pub fn caption_reconciliation(&self) -> Option<CaptionReconciliation> {
        self.captions.as_ref().map(CaptionVerifier::reconciliation)
    }

    /// Video tracks carrying captions, over the number that should.
    pub fn caption_carriage(&self) -> Option<(usize, usize)> {
        self.captions.as_ref().map(CaptionVerifier::carriage)
    }

    /// The channels currently declared, for reporting.
    pub fn caption_channels(&self) -> Vec<CaptionChannel> {
        self.captions
            .as_ref()
            .map(CaptionVerifier::declared_channels)
            .unwrap_or_default()
    }

    /// Malformed SEI messages ignored across every video track.
    pub fn caption_malformed_sei(&self) -> u64 {
        self.captions
            .as_ref()
            .map_or(0, CaptionVerifier::malformed_sei)
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
                // Inspected before normalization, while the access unit is
                // still exactly the bytes the publisher sent. Read-only: the
                // packet continues into the pipeline untouched.
                if let Some(captions) = &mut self.captions
                    && let Some(services) =
                        captions.inspect(packet.track_id, packet.payload.as_bytes())
                {
                    self.declared_captions = Some(services);
                }
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
            self.density
                .admit(consumed.packets as u64, &self.normalized)?;
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
        self.publish()
    }

    fn write_one(&mut self, sample: NormalizedSample) -> Result<(), ExecutionError> {
        self.muxer.push(sample, &mut self.media)?;
        self.publish()
    }

    /// Advertises in-band caption services on the live publication.
    fn declare_closed_captions(&mut self, services: Arc<[ClosedCaptionService]>) -> bool {
        self.publisher.declare_closed_captions(services)
    }

    fn finish(&mut self, reason: FinishReason) -> Result<(), ExecutionError> {
        self.muxer.finish(reason, &mut self.media)?;
        self.publish()?;
        self.publisher.finish(reason)?;
        Ok(())
    }

    /// Hands everything the muxer produced to the publisher, counting as it
    /// goes.
    ///
    /// Muxed and published volume are tallied in the same pass. They are
    /// different numbers — a superseded write is muxed but never delivered —
    /// but both are known at the moment an object is handed over, and walking
    /// the buffer twice to learn them separately only invited the two counts to
    /// be taken from different states of it.
    fn publish(&mut self) -> Result<(), ExecutionError> {
        let mut muxed = MediaCounts::default();
        let mut delivered = MediaCounts::default();
        let mut result = Ok(());

        for media in self.media.drain(..) {
            let counts = MediaCounts::of(&media);
            muxed += counts;
            match self.publisher.write(media) {
                // Packaging calls these chunks; HLS projects each one as a
                // partial segment and therefore reports it as a part.
                Ok(PublishOutcome::Published) => delivered += counts,
                Ok(PublishOutcome::Superseded) => {}
                Err(error) => {
                    result = Err(error.into());
                    break;
                }
            }
        }
        self.media.clear();

        // Both reported even when publication failed part way, so the counters
        // reflect what was actually produced and what viewers actually got.
        self.mux_meters.mux_progress(muxed.chunks, muxed.segments);
        self.delivery_meters
            .delivery_progress(delivered.chunks, delivered.segments);
        result
    }
}

/// The media objects one packaged event represents.
#[derive(Clone, Copy, Debug, Default)]
struct MediaCounts {
    chunks: u64,
    segments: u64,
}

impl MediaCounts {
    /// Initialization events produce no media object. A direct segment and a
    /// chunked segment completion each represent one completed segment, while
    /// the completion carries no second copy of its payload.
    fn of(media: &PackagedMedia) -> Self {
        let (chunks, segments) = match media {
            PackagedMedia::Initialization(_) => (0, 0),
            PackagedMedia::Chunk(_) => (1, 0),
            PackagedMedia::Segment(_) | PackagedMedia::SegmentCompleted(_) => (0, 1),
        };
        Self { chunks, segments }
    }
}

impl std::ops::AddAssign for MediaCounts {
    fn add_assign(&mut self, other: Self) {
        self.chunks = self.chunks.saturating_add(other.chunks);
        self.segments = self.segments.saturating_add(other.segments);
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
    /// The last caption state reported, so each transition is announced once.
    reported_captions: Option<CaptionReconciliation>,
    /// Malformed SEI already reported, so a rising count is announced once.
    reported_malformed_sei: u64,
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
            reported_captions: None,
            reported_malformed_sei: 0,
        }
    }

    /// Applies any queued caption declaration and announces a state change.
    ///
    /// One place, so the replay branch and the steady-state loop cannot drift:
    /// both have to declare before writing media, and pre-roll makes the replay
    /// branch the only chance a short input ever gets.
    fn publish_captions(&mut self, events: &EventSink) {
        if let Some(services) = self.head.take_caption_declaration() {
            self.tail.declare_closed_captions(services);
        }
        self.report_captions(events);
    }

    /// Announces a change in what the video tracks were observed to carry.
    ///
    /// Driven from the pump rather than the detector so the event is emitted
    /// once per transition, not once per access unit that confirms it.
    fn report_captions(&mut self, events: &EventSink) {
        let Some(state) = self.head.caption_reconciliation() else {
            return;
        };
        if self.reported_captions == Some(state) {
            return;
        }
        self.reported_captions = Some(state);
        let Some((carrying, video_tracks)) = self.head.caption_carriage() else {
            return;
        };
        match state {
            CaptionReconciliation::Consistent => {
                events.emit(SessionEvent::ClosedCaptionsDetected {
                    channels: self
                        .head
                        .caption_channels()
                        .into_iter()
                        .map(|channel| channel.to_string())
                        .collect(),
                });
            }
            CaptionReconciliation::PartialLadder => {
                events.emit(SessionEvent::ClosedCaptionsPartial {
                    carrying,
                    video_tracks,
                });
            }
            CaptionReconciliation::ChannelMismatch => {
                events.emit(SessionEvent::ClosedCaptionsChannelMismatch);
            }
            // Nothing observed yet is the state every publication starts in,
            // so it is not an event.
            CaptionReconciliation::Absent => {}
        }

        // Reported alongside the state change rather than on its own schedule:
        // malformed SEI matters because it means the caption picture may be
        // incomplete, which is only actionable next to what was concluded.
        let malformed = self.head.caption_malformed_sei();
        if malformed > self.reported_malformed_sei {
            self.reported_malformed_sei = malformed;
            events.emit(SessionEvent::ClosedCaptionsMalformedSei {
                messages: malformed,
            });
        }
    }

    /// Advances the pipeline by one batch, preserving how input ended.
    ///
    /// Stepping a batch at a time is what lets supervision interleave stop
    /// checks and health evaluation without ever cancelling a partially
    /// processed read.
    pub async fn pump(&mut self, events: &EventSink) -> Result<InputState, ExecutionError> {
        if !self.replayed {
            self.replayed = true;
            // Pre-roll drove the same `MediaHead`, so its access units have
            // already been inspected and may have queued a declaration. It has
            // to be applied *before* the buffered media is written: the first
            // write is what wakes viewers blocked on the multivariant playlist,
            // and for an input that ended during pre-roll there is no later
            // pump to apply it at all.
            self.publish_captions(events);
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

        // Declared before the samples are written so the multivariant playlist
        // gains the caption group no later than the media it describes. The
        // transition is rare — once per publication in the ordinary case — so
        // this costs a check per batch and nothing more.
        self.publish_captions(events);

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

    /// What a publisher saw, in the order it saw it.
    #[derive(Default)]
    struct PublishLog {
        /// Media objects written before any caption declaration arrived.
        media_before_captions: usize,
        declared: bool,
    }

    /// Records whether captions were declared before any media was published.
    struct OrderedPublisher(Arc<parking_lot::Mutex<PublishLog>>);

    impl HlsPublisher for OrderedPublisher {
        fn write(&mut self, _media: PackagedMedia) -> Result<PublishOutcome, HlsError> {
            let mut log = self.0.lock();
            if !log.declared {
                log.media_before_captions += 1;
            }
            Ok(PublishOutcome::Published)
        }

        fn declare_closed_captions(&mut self, _services: Arc<[ClosedCaptionService]>) -> bool {
            self.0.lock().declared = true;
            true
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

    /// A source that is already exhausted, standing in for an input that ended
    /// during pre-roll.
    struct ExhaustedSource;

    impl PacketSource for ExhaustedSource {
        fn discover(
            &mut self,
            _limits: crate::source::DiscoveryLimits,
        ) -> BoxFuture<'_, Result<crate::source::DiscoveryReport, crate::source::SourceError>>
        {
            unreachable!("discovery already happened")
        }

        fn fill<'a>(
            &'a mut self,
            _out: &'a mut dyn Appender<Packet>,
        ) -> BoxFuture<'a, Result<InputState, crate::source::SourceError>> {
            Box::pin(async { Ok(InputState::Closed) })
        }
    }

    struct IdleNormalizer;

    impl MediaNormalizer for IdleNormalizer {
        fn push(
            &mut self,
            _packet: Packet,
            _out: &mut dyn Appender<NormalizedSample>,
        ) -> Result<(), crate::media::NormalizeError> {
            Ok(())
        }

        fn finish(
            &mut self,
            _out: &mut dyn Appender<NormalizedSample>,
        ) -> Result<(), crate::media::NormalizeError> {
            Ok(())
        }
    }

    #[tokio::test]
    async fn captions_seen_during_preroll_are_declared_before_any_media() {
        // Pre-roll drives the same `MediaHead`, so a short captioned input can
        // have its captions detected and its input closed before the live loop
        // ever runs. The replay branch is then the only chance to declare them:
        // if it publishes media first, viewers are woken with a captionless
        // multivariant playlist, and if the input ended there is no later pump
        // at all.
        let session = SessionMeters::new(ProcessMeters::default());
        let mut head = MediaHead::new(
            Box::new(ExhaustedSource),
            Box::new(IdleNormalizer),
            session.media_view(),
            InputLimits::permissive(),
            &crate::media::fixtures::video_timeline(),
            None,
        );
        head.declared_captions = Some(Arc::from([ClosedCaptionService {
            channel: crate::mux::CaptionChannel::Cea708Service(1),
            name: Arc::from("Service 1"),
            language: None,
            is_default: true,
            autoselect: true,
        }]));

        let log = Arc::new(parking_lot::Mutex::new(PublishLog::default()));
        let publisher = Box::new(OrderedPublisher(Arc::clone(&log)));
        let tail = MediaTail::new(
            Box::new(IdleMuxer),
            publisher,
            session.mux_view(),
            session.delivery_view(),
        );
        let pacer = MediaPacer::after_preroll(
            None,
            None,
            std::time::Duration::from_mins(1),
            &crate::media::fixtures::video_timeline(),
            &[],
            session.media_view(),
        )
        .expect("an empty pre-roll paces");

        let mut live = LiveSession::new(
            head,
            tail,
            pacer,
            vec![crate::media::fixtures::video_sample_at(0)],
            // The publisher already went away, which is the edge that loses the
            // declaration entirely.
            InputState::Closed,
        );
        let events =
            crate::observe::Events::default().scoped(crate::domain::SessionId(nz::u64!(1)));
        live.pump(&events).await.expect("the replay pump succeeds");

        let log = log.lock();
        assert!(
            log.declared,
            "captions detected during pre-roll must still be declared"
        );
        assert_eq!(
            log.media_before_captions, 0,
            "captions must be declared before the first media write"
        );
    }
}
