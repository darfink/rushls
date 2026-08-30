use std::{collections::VecDeque, sync::Arc, time::Duration};

use parking_lot::Mutex;

use crate::{
    admission::{
        AdmissionError, Authenticator, ClientInfo, IngestProtocol, PresentedCredential, Principal,
        PublishGrant, PublishRequest, PublishResource, StreamPolicy, TakeoverPolicy,
    },
    delivery::hls::{
        HlsError, HlsPublisher, PublishOutcome, PublisherFactory, StorePublisherFactory,
        StreamStore,
    },
    delivery::store::StoreWriteError,
    domain::{
        Appender, BoxFuture, Codec, MediaKind, Payload, RenditionId, SessionId, StreamId, Timebase,
        TrackCounts, TrackId, fixtures::video_catalog,
    },
    media::{
        MediaNormalizer, NormalizeError, NormalizedSample, NormalizerFactory, PresentationPlan,
        StartedNormalizer, TimelineCalibration, VideoSample,
    },
    mux::{
        FinishReason, InitializationSegment, MuxError, Muxer, MuxerFactory, MuxerStartRequest,
        PackagedChunk, PackagedMedia, PackagedPresentation, PackagedSegmentCompletion,
        PackagingRenditionId, PackagingSegmentId, StartedMuxer, fixtures as mux_fixtures,
        fixtures::RenditionBuilder,
    },
    observe::{EventObserver, Events, ProcessMeters, SessionEnd, SessionEvent, SourceMeters},
    segment::{PrerollLimits, SegmentationPolicy},
    source::{
        AcceptedPublish, BatchUnit, DensityUnit, DiscoveryLimits, DiscoveryReport, InputLimits,
        InputState, LimitError, Packet, PacketSource, PendingPublish, PublishRejection,
        SourceError, TransportError,
    },
};

use super::{
    AtCapacity, ExecutionError, HealthEvaluation, HealthPolicy, PendingPermit, PendingPublishers,
    Registry, RegistryError, Services, SessionConfig, SessionError, SessionOutcome, StopReason,
    SupervisionError, SupervisionPolicy, run_session,
};

const SECOND: i64 = 90_000;

type CallLog = Arc<Mutex<Vec<&'static str>>>;
type FinishLog = Arc<Mutex<Vec<FinishReason>>>;

/// Ways a collaborator can misbehave, so the spine's response can be tested
/// rather than reasoned about.
#[derive(Clone, Copy, Debug, Default)]
#[allow(clippy::struct_excessive_bools)]
struct Faults {
    /// The muxer cannot close out its container.
    muxer_finish_fails: bool,
    /// The normalizer holds one sample back for reordering, releasing it only
    /// when it is finished.
    normalizer_reorders: bool,
    /// The muxer's `push` fails once this many pushes have succeeded.
    muxer_push_fails_after: Option<u32>,
    /// The publisher's `write` fails once this many writes have succeeded.
    publisher_fails_after: Option<u32>,
    /// The muxer emits the second chunk of a segment with index 2 instead of 1.
    muxer_skips_chunk_index: bool,
    /// The muxer accepts every sample but emits nothing, as a defective
    /// packager would.
    muxer_swallows_samples: bool,
    /// The source panics on the fill whose delivery count matches.
    source_panics_after: Option<u32>,
}

fn record(log: &CallLog, call: &'static str) {
    log.lock().push(call);
}

fn stream() -> StreamId {
    StreamId::new("live/camera")
}

/// How a scripted source behaves once its prepared batches run out.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Ending {
    Eof,
    Interrupted,
    Stall,
    CodecChange,
    /// Never finishes probing, so the spine's discovery deadline is the only
    /// thing that can end the session.
    HangOnDiscovery,
    /// Fails with a source error once this many batches were delivered.
    FailAfter(u32),
    /// Reports a clean end of input while still holding media back.
    LyingEof,
    /// Delivers one frozen-clock packet per fill, forever.
    Endless,
}

struct FakeSource {
    log: CallLog,
    meters: Arc<dyn SourceMeters>,
    batches: VecDeque<Vec<Packet>>,
    ending: Ending,
    panics_after: Option<u32>,
    delivered: u32,
}

impl PacketSource for FakeSource {
    fn discover(
        &mut self,
        _limits: DiscoveryLimits,
    ) -> BoxFuture<'_, Result<DiscoveryReport, SourceError>> {
        record(&self.log, "discover");
        let ending = self.ending;
        Box::pin(async move {
            if ending == Ending::HangOnDiscovery {
                std::future::pending::<()>().await;
            }
            Ok(DiscoveryReport {
                tracks: video_catalog(),
            })
        })
    }

    fn fill<'a>(
        &'a mut self,
        out: &'a mut dyn Appender<Packet>,
    ) -> BoxFuture<'a, Result<InputState, SourceError>> {
        record(&self.log, "fill");
        Box::pin(async move {
            assert!(
                self.panics_after != Some(self.delivered),
                "injected source panic"
            );
            if matches!(self.ending, Ending::FailAfter(limit) if self.delivered >= limit) {
                return Err(SourceError::Input("injected source failure".into()));
            }
            if matches!(self.ending, Ending::Endless) {
                if self.delivered == 0 {
                    // The same first batch as the scripted fixture, so
                    // pre-roll locks on it.
                    let cycle = [
                        packet(0, false),
                        packet(SECOND, true),
                        packet(2 * SECOND, false),
                    ];
                    let payload_bytes: u64 = cycle.iter().map(|p| p.payload.len() as u64).sum();
                    for opening in cycle {
                        out.push(opening);
                    }
                    self.meters.source_progress(payload_bytes, 3, 0);
                } else {
                    // One new packet per fill afterwards, on a realtime
                    // cadence: the pacer lets one through per second, so
                    // source and media stay fresh while the swallowing muxer
                    // never publishes anything. A frozen clock would be caught
                    // by the media-density limiter instead, which is a
                    // different defence.
                    let pts = i64::from(self.delivered)
                        .saturating_add(2)
                        .saturating_mul(SECOND);
                    let next = packet(pts, false);
                    let payload_bytes = next.payload.len() as u64;
                    out.push(next);
                    self.meters.source_progress(payload_bytes, 1, 0);
                }
                self.delivered = self.delivered.saturating_add(1);
                return Ok(InputState::Open);
            }
            if let Some(batch) = self.batches.pop_front() {
                let bytes = batch.iter().map(|packet| packet.payload.len() as u64).sum();
                let packets = batch.len() as u64;
                for packet in batch {
                    out.push(packet);
                }
                self.meters.source_progress(bytes, packets, 0);
                self.delivered = self.delivered.saturating_add(1);
                let state = if self.batches.is_empty() {
                    match self.ending {
                        Ending::Eof | Ending::LyingEof => InputState::Closed,
                        Ending::Interrupted => InputState::Interrupted,
                        _ => InputState::Open,
                    }
                } else if matches!(self.ending, Ending::LyingEof) {
                    // A lying source reports end of input while still holding
                    // batches; the pipeline must never be asked to read them.
                    InputState::Closed
                } else {
                    InputState::Open
                };
                return Ok(state);
            }

            match self.ending {
                Ending::Eof | Ending::HangOnDiscovery | Ending::LyingEof => Ok(InputState::Closed),
                Ending::Interrupted => Ok(InputState::Interrupted),
                Ending::Stall => std::future::pending().await,
                Ending::CodecChange => {
                    self.meters.codec_parameters_changed();
                    Err(SourceError::CodecParametersChanged {
                        track_id: TrackId(0),
                    })
                }
                Ending::FailAfter(_) => Err(SourceError::Input("injected source failure".into())),
                Ending::Endless => unreachable!("the endless branch returns above"),
            }
        })
    }
}

struct FakePending {
    log: CallLog,
    resource: &'static str,
    batches: Vec<Vec<Packet>>,
    ending: Ending,
    panics_after: Option<u32>,
}

impl PendingPublish for FakePending {
    fn publish_request(&self) -> Result<PublishRequest, TransportError> {
        record(&self.log, "publish_request");
        Ok(PublishRequest {
            protocol: IngestProtocol::Rtmp,
            resource: PublishResource {
                namespace: Some("live".into()),
                name: self.resource.into(),
            },
            credential: PresentedCredential::new("secret"),
            client: ClientInfo {
                remote_address: "127.0.0.1:1935".parse().expect("constant address is valid"),
                encoder: Some("test-encoder".into()),
                protocol_version: None,
            },
        })
    }

    fn accept(
        self: Box<Self>,
        grant: PublishGrant,
        meters: Arc<dyn SourceMeters>,
    ) -> BoxFuture<'static, Result<AcceptedPublish, TransportError>> {
        record(&self.log, "accept");
        Box::pin(async move {
            Ok(AcceptedPublish {
                source: Box::new(FakeSource {
                    log: self.log.clone(),
                    meters,
                    batches: self.batches.into(),
                    ending: self.ending,
                    panics_after: self.panics_after,
                    delivered: 0,
                }),
                grant,
            })
        })
    }

    fn reject(
        self: Box<Self>,
        _rejection: PublishRejection,
    ) -> BoxFuture<'static, Result<(), TransportError>> {
        record(&self.log, "reject");
        Box::pin(async move { Ok(()) })
    }
}

struct FakeAuthenticator {
    log: CallLog,
    rejection: Option<AdmissionError>,
    policy: StreamPolicy,
}

impl Authenticator for FakeAuthenticator {
    fn authenticate<'a>(
        &'a self,
        request: &'a PublishRequest,
    ) -> BoxFuture<'a, Result<PublishGrant, AdmissionError>> {
        record(&self.log, "authenticate");
        Box::pin(async move {
            if let Some(error) = &self.rejection {
                return Err(error.clone());
            }
            Ok(PublishGrant {
                stream_id: StreamId::new(format!(
                    "{}/{}",
                    request.resource.namespace.as_deref().unwrap_or_default(),
                    request.resource.name
                )),
                principal: Principal("publisher-1".into()),
                policy: self.policy.clone(),
            })
        })
    }
}

struct FakeNormalizerFactory {
    log: CallLog,
    faults: Faults,
}

impl NormalizerFactory for FakeNormalizerFactory {
    fn start(
        &self,
        presentation: &PresentationPlan,
        timeline: &TimelineCalibration,
    ) -> Result<StartedNormalizer, NormalizeError> {
        record(&self.log, "normalizer_start");
        Ok(StartedNormalizer {
            normalizer: Box::new(FakeNormalizer {
                reorders: self.faults.normalizer_reorders,
                held: None,
            }),
            presentation: presentation.clone(),
            timeline: timeline.clone(),
        })
    }
}

/// Optionally holds one sample back, standing in for a real reorder buffer.
struct FakeNormalizer {
    reorders: bool,
    held: Option<NormalizedSample>,
}

impl MediaNormalizer for FakeNormalizer {
    fn push(
        &mut self,
        packet: Packet,
        out: &mut dyn Appender<NormalizedSample>,
    ) -> Result<(), NormalizeError> {
        let pts = packet
            .pts
            .ok_or_else(|| NormalizeError::Processing("test packet has no PTS".into()))?;
        let sample = NormalizedSample::Video(VideoSample {
            track_id: packet.track_id,
            codec: Codec::H264,
            pts,
            dts: packet.dts.unwrap_or(pts),
            duration: packet.duration.unwrap_or(SECOND).cast_unsigned(),
            random_access: packet.random_access,
            payload: packet.payload,
        });

        if !self.reorders {
            out.push(sample);
            return Ok(());
        }
        if let Some(previous) = self.held.replace(sample) {
            out.push(previous);
        }
        Ok(())
    }

    fn finish(&mut self, out: &mut dyn Appender<NormalizedSample>) -> Result<(), NormalizeError> {
        if let Some(held) = self.held.take() {
            out.push(held);
        }
        Ok(())
    }
}

struct FakeMuxerFactory {
    log: CallLog,
    finished: FinishLog,
    faults: Faults,
}

impl MuxerFactory for FakeMuxerFactory {
    fn start(&self, request: MuxerStartRequest<'_>) -> Result<StartedMuxer, MuxError> {
        record(&self.log, "muxer_start");
        // One-second chunks accumulated into a single segment closed at end of
        // stream. This fake never cuts mid-publication, so its declared target
        // covers the whole scripted run rather than a live cadence: delivery
        // holds a rendition to the target it advertises, and a fixture that
        // under-declared would be refused for a cadence it never had.
        let presentation = mux_fixtures::presentation_at(
            request.time_anchor,
            request.presentation,
            vec![
                RenditionBuilder::new(0, MediaKind::Video)
                    .key("video/main")
                    .config(mux_fixtures::config(
                        Timebase::hz90k(),
                        4 * SECOND as u64,
                        Some(SECOND as u64),
                    ))
                    .build(),
            ],
        );
        Ok(StartedMuxer {
            muxer: Box::new(FakeMuxer {
                initialized: false,
                chunks: 0,
                pushes: 0,
                duration: 0,
                finished: Arc::clone(&self.finished),
                faults: self.faults,
            }),
            presentation: Arc::new(presentation),
        })
    }
}

/// Emits one part per sample and closes a single segment at end of stream.
struct FakeMuxer {
    initialized: bool,
    chunks: u32,
    pushes: u32,
    duration: u64,
    finished: FinishLog,
    faults: Faults,
}

impl Muxer for FakeMuxer {
    fn expected_publication_interval(&self) -> Duration {
        Duration::from_secs(1)
    }

    fn push(
        &mut self,
        sample: NormalizedSample,
        out: &mut dyn Appender<PackagedMedia>,
    ) -> Result<(), MuxError> {
        if self.faults.muxer_swallows_samples {
            return Ok(());
        }
        if self.faults.muxer_push_fails_after == Some(self.pushes) {
            return Err(MuxError::Mux("injected push failure".into()));
        }
        self.pushes = self.pushes.saturating_add(1);
        if !self.initialized {
            self.initialized = true;
            out.push(PackagedMedia::Initialization(InitializationSegment {
                rendition_id: PackagingRenditionId(0),
                version: 1,
                payload: Payload::from(vec![0]),
            }));
        }
        out.push(PackagedMedia::Chunk(PackagedChunk {
            rendition_id: PackagingRenditionId(0),
            packaging_segment_id: PackagingSegmentId(0),
            // A skipping muxer emits index 2 where 1 belongs, which the store
            // must refuse before the segment can be assembled around a hole.
            chunk_index: self.chunks
                + u32::from(self.faults.muxer_skips_chunk_index && self.chunks == 1),
            media_start: sample.pts(),
            duration: sample.duration(),
            independent: sample.random_access(),
            payload: Payload::from(vec![1]),
        }));
        self.chunks = self.chunks.saturating_add(1);
        self.duration = self.duration.saturating_add(sample.duration());
        Ok(())
    }

    fn finish(
        &mut self,
        reason: FinishReason,
        out: &mut dyn Appender<PackagedMedia>,
    ) -> Result<(), MuxError> {
        self.finished.lock().push(reason);
        if self.faults.muxer_finish_fails {
            return Err(MuxError::Mux("container state is inconsistent".into()));
        }
        // A successor is about to publish; a one-frame trailing segment would
        // only be something for it to be discontinuous with.
        if matches!(reason, FinishReason::Superseded) {
            return Ok(());
        }
        if self.initialized {
            out.push(PackagedMedia::SegmentCompleted(PackagedSegmentCompletion {
                rendition_id: PackagingRenditionId(0),
                packaging_segment_id: PackagingSegmentId(0),
                media_start: 0,
                duration: self.duration,
            }));
        }
        Ok(())
    }
}

/// Wraps the real store publisher so a session can be handed a write failure.
struct FaultyPublisherFactory {
    inner: StorePublisherFactory,
    faults: Faults,
}

impl PublisherFactory for FaultyPublisherFactory {
    fn start(
        &self,
        stream: &StreamId,
        presentation: Arc<PackagedPresentation>,
    ) -> Result<Box<dyn HlsPublisher>, HlsError> {
        let inner = self.inner.start(stream, presentation)?;
        Ok(Box::new(FaultyPublisher {
            inner,
            writes: 0,
            faults: self.faults,
        }))
    }
}

/// Delegates to the real publisher until the injected failure count is hit.
struct FaultyPublisher {
    inner: Box<dyn HlsPublisher>,
    writes: u32,
    faults: Faults,
}

impl HlsPublisher for FaultyPublisher {
    fn write(&mut self, media: PackagedMedia) -> Result<PublishOutcome, HlsError> {
        if self.faults.publisher_fails_after == Some(self.writes) {
            return Err(HlsError::Store(StoreWriteError::PayloadCapacityExceeded {
                maximum: 0,
                additional: 1,
            }));
        }
        self.writes = self.writes.saturating_add(1);
        self.inner.write(media)
    }

    fn finish(&mut self, reason: FinishReason) -> Result<(), HlsError> {
        self.inner.finish(reason)
    }
}

#[derive(Default)]
struct Recorder {
    events: Mutex<Vec<SessionEvent>>,
}

impl EventObserver for Recorder {
    fn observe(&self, _session: SessionId, event: SessionEvent) {
        self.events.lock().push(event);
    }
}

impl Recorder {
    fn names(&self) -> Vec<&'static str> {
        self.events.lock().iter().map(name).collect()
    }
}

fn name(event: &SessionEvent) -> &'static str {
    match event {
        SessionEvent::Accepted { .. } => "accepted",
        SessionEvent::Displaced { .. } => "displaced",
        SessionEvent::TracksDiscovered { .. } => "tracks_discovered",
        SessionEvent::TimelineCalibrated { .. } => "timeline_calibrated",
        SessionEvent::SegmentationLocked { .. } => "segmentation_locked",
        SessionEvent::SegmentationExtended { .. } => "segmentation_extended",
        SessionEvent::SubtitleCueTooLate { .. } => "subtitle_cue_too_late",
        SessionEvent::SubtitleStateLongLived { .. } => "subtitle_state_long_lived",
        SessionEvent::ClosedCaptionsDetected { .. } => "closed_captions_detected",
        SessionEvent::ClosedCaptionsPartial { .. } => "closed_captions_partial",
        SessionEvent::ClosedCaptionsChannelMismatch => "closed_captions_channel_mismatch",
        SessionEvent::ClosedCaptionsMalformedSei { .. } => "closed_captions_malformed_sei",
        SessionEvent::Running => "running",
        SessionEvent::TrackSetChanged => "track_set_changed",
        SessionEvent::CodecParametersChanged { .. } => "codec_parameters_changed",
        SessionEvent::Unhealthy { .. } => "unhealthy",
        SessionEvent::Draining => "draining",
        SessionEvent::DrainFailed { .. } => "drain_failed",
        SessionEvent::Ended { .. } => "ended",
        SessionEvent::Failed { .. } => "failed",
    }
}

fn packet(pts: i64, random_access: bool) -> Packet {
    Packet {
        track_id: TrackId(0),
        pts: Some(pts),
        dts: Some(pts),
        duration: Some(SECOND),
        random_access,
        audio_trim: crate::domain::AudioTrim::default(),
        webvtt: crate::domain::WebVttCueMetadata::default(),
        subtitle_position: None,
        payload: Payload::from(vec![1, 2, 3]),
    }
}

/// Enough media for pre-roll to prove a boundary at one second, plus one more
/// batch that only the live loop sees.
fn scripted_batches() -> Vec<Vec<Packet>> {
    vec![
        vec![
            packet(0, false),
            packet(SECOND, true),
            packet(2 * SECOND, false),
        ],
        vec![packet(3 * SECOND, false)],
    ]
}

fn config() -> SessionConfig {
    SessionConfig {
        maximum_admission_time: Duration::from_secs(5),
        discovery: DiscoveryLimits {
            maximum_probe_bytes: 1_048_576,
            maximum_wall_time: Duration::from_secs(5),
        },
        input: InputLimits::permissive(),
        preroll: PrerollLimits {
            maximum_buffered_bytes: 8_388_608,
            maximum_buffered_samples: 4_096,
            maximum_wall_time: Duration::from_secs(5),
            maximum_media_duration: Duration::from_secs(30),
        },
        segmentation: SegmentationPolicy::latency_first(
            Duration::from_secs(2),
            Duration::from_millis(200),
        ),
        // Long enough that liveness never fires unless a test asks it to.
        supervision: SupervisionPolicy {
            health: HealthPolicy {
                source_stall_timeout: Duration::from_hours(1),
                media_stall_timeout: Duration::from_hours(1),
                stalled_publication_multiplier: 1_000,
                minimum_publication_stall_tolerance: Duration::from_hours(1),
            },
            health_interval: Duration::from_hours(1),
        },
    }
}

struct Harness {
    services: Services,
    log: CallLog,
    finished: FinishLog,
    recorder: Arc<Recorder>,
    store: StreamStore,
    meters: ProcessMeters,
    sessions: Registry,
    panics_after: Option<u32>,
}

impl Harness {
    fn new(rejection: Option<AdmissionError>, policy: StreamPolicy, faults: Faults) -> Self {
        let log = CallLog::default();
        let finished = FinishLog::default();
        let recorder = Arc::new(Recorder::default());
        let store = StreamStore::default();
        let meters = ProcessMeters::default();
        let sessions = Registry::default();

        let services = Services {
            authenticator: Arc::new(FakeAuthenticator {
                log: log.clone(),
                rejection,
                policy,
            }),
            normalizers: Arc::new(FakeNormalizerFactory {
                log: log.clone(),
                faults,
            }),
            muxers: Arc::new(FakeMuxerFactory {
                log: log.clone(),
                finished: Arc::clone(&finished),
                faults,
            }),
            publishers: Arc::new(FaultyPublisherFactory {
                inner: StorePublisherFactory::new(store.clone()),
                faults,
            }),
            sessions: sessions.clone(),
            meters: meters.clone(),
            events: Events::new(Arc::clone(&recorder) as Arc<dyn EventObserver>),
        };

        Self {
            services,
            log,
            finished,
            recorder,
            store,
            meters,
            sessions,
            panics_after: faults.source_panics_after,
        }
    }

    fn healthy() -> Self {
        Self::new(None, StreamPolicy::permissive(), Faults::default())
    }

    fn faulty(faults: Faults) -> Self {
        Self::new(None, StreamPolicy::permissive(), faults)
    }

    fn pending(&self, ending: Ending) -> Box<dyn PendingPublish> {
        self.pending_for("camera", ending)
    }

    fn pending_for(&self, resource: &'static str, ending: Ending) -> Box<dyn PendingPublish> {
        Box::new(FakePending {
            log: self.log.clone(),
            resource,
            batches: scripted_batches(),
            ending,
            panics_after: self.panics_after,
        })
    }

    async fn run(&self, ending: Ending) -> Result<SessionOutcome, SessionError> {
        run_session(
            self.pending(ending),
            &self.services,
            &config(),
            PendingPermit::unlimited(),
        )
        .await
    }

    async fn run_with(
        &self,
        ending: Ending,
        config: &SessionConfig,
    ) -> Result<SessionOutcome, SessionError> {
        run_session(
            self.pending(ending),
            &self.services,
            config,
            PendingPermit::unlimited(),
        )
        .await
    }

    fn calls(&self) -> Vec<&'static str> {
        self.log.lock().clone()
    }

    fn finish_reasons(&self) -> Vec<FinishReason> {
        self.finished.lock().clone()
    }

    /// The stream this harness publishes, whether or not it is still leased.
    fn live(&self) -> Arc<crate::delivery::hls::LiveStream> {
        self.store.get(&stream()).expect("the stream is retained")
    }
}

#[tokio::test]
async fn a_publication_runs_through_every_stage_and_releases_everything_it_held() {
    let harness = Harness::healthy();

    let outcome = harness.run(Ending::Eof).await.expect("session succeeds");

    assert_eq!(outcome, SessionOutcome::Ended);
    assert_eq!(
        harness.recorder.names(),
        [
            "accepted",
            "tracks_discovered",
            "timeline_calibrated",
            "segmentation_locked",
            "running",
            "draining",
            "ended",
        ]
    );
    assert_eq!(
        harness.calls(),
        [
            "publish_request",
            "authenticate",
            "accept",
            "discover",
            "normalizer_start",
            // Pre-roll drives the head directly; the muxer only exists once
            // segmentation has locked. The read that reports end of input is
            // the last one: nothing polls a source it already drained.
            "fill",
            "muxer_start",
            "fill",
        ]
    );

    let totals = harness.meters.snapshot();
    assert_eq!(totals.sessions_started, 1);
    assert_eq!(totals.sessions_completed, 1);
    assert_eq!(totals.sessions_failed, 0);
    // Three samples buffered during pre-roll plus one seen live, then one
    // segment closed on drain.
    assert_eq!(totals.parts_published, 4);
    assert_eq!(totals.segments_published, 1);

    assert_eq!(harness.finish_reasons(), [FinishReason::Final]);
    assert!(harness.sessions.is_empty());
    assert_eq!(harness.store.leased(), 0, "the write lease is released");
    assert!(
        harness.live().is_ended(),
        "an input that ended for good ends its stream"
    );
}

#[tokio::test]
async fn an_interrupted_input_flushes_media_without_ending_the_stream() {
    let harness = Harness::healthy();

    let outcome = harness
        .run(Ending::Interrupted)
        .await
        .expect("an interrupted input drains successfully");

    assert_eq!(outcome, SessionOutcome::Interrupted);
    assert_eq!(harness.finish_reasons(), [FinishReason::Interrupted]);
    assert!(
        !harness.live().is_ended(),
        "delivery remains resumable while the publisher reconnects"
    );
    let ended = harness
        .recorder
        .events
        .lock()
        .iter()
        .filter_map(|event| match event {
            SessionEvent::Ended { end } => Some(*end),
            _ => None,
        })
        .next_back();
    assert_eq!(ended, Some(SessionEnd::Interrupted));
}

#[tokio::test]
async fn preroll_media_reaches_delivery_rather_than_being_dropped() {
    let harness = Harness::healthy();
    harness.run(Ending::Eof).await.expect("session succeeds");

    let session = harness.meters.snapshot();
    assert_eq!(session.packets_received, 4);
    assert_eq!(session.parts_published, 4);
}

#[tokio::test]
async fn a_failed_handshake_is_turned_away_at_the_transport_boundary() {
    let harness = Harness::new(
        Some(AdmissionError::InvalidCredential),
        StreamPolicy::permissive(),
        Faults::default(),
    );

    let error = harness
        .run(Ending::Eof)
        .await
        .expect_err("authentication fails");

    assert_eq!(
        error,
        SessionError::Admission(AdmissionError::InvalidCredential)
    );
    assert_eq!(
        harness.calls(),
        ["publish_request", "authenticate", "reject"]
    );

    let totals = harness.meters.snapshot();
    assert_eq!(totals.publishers_rejected, 1);
    // A publication that was never admitted is not a failed session.
    assert_eq!(totals.sessions_started, 0);
    assert_eq!(totals.sessions_failed, 0);
    assert!(harness.recorder.names().is_empty());
}

#[tokio::test]
async fn a_disallowed_codec_ends_the_session_before_any_media_is_planned() {
    let harness = Harness::new(
        None,
        StreamPolicy {
            accepted_video_codecs: vec![Codec::Av1],
            ..StreamPolicy::permissive()
        },
        Faults::default(),
    );

    let error = harness
        .run(Ending::Eof)
        .await
        .expect_err("validation rejects the presentation");

    assert!(matches!(error, SessionError::Validation(_)));
    assert_eq!(
        harness.calls(),
        ["publish_request", "authenticate", "accept", "discover"]
    );
    assert_eq!(harness.recorder.names(), ["accepted", "failed"]);
    assert_eq!(harness.meters.snapshot().sessions_failed, 1);
    assert!(harness.sessions.is_empty());
}

#[tokio::test]
async fn a_midstream_codec_change_is_counted_by_the_source_that_saw_it() {
    let harness = Harness::healthy();

    let error = harness
        .run(Ending::CodecChange)
        .await
        .expect_err("a codec change terminates the session");

    assert!(matches!(
        error,
        SessionError::Supervision(SupervisionError::Execution(_))
    ));

    let totals = harness.meters.snapshot();
    assert_eq!(totals.sessions_failed, 1);
    assert_eq!(totals.codec_parameter_changes, 1);
    assert_eq!(totals.drain_failures, 0);
    assert!(harness.sessions.is_empty());
    assert_eq!(harness.store.leased(), 0);
}

#[tokio::test]
async fn an_admission_slot_is_returned_before_the_session_it_admitted_ends() {
    let harness = Harness::healthy();
    let services = harness.services.clone();
    let pending = harness.pending(Ending::Stall);
    // One slot, so a session still holding it would make the budget observably
    // empty for as long as the publisher stays connected.
    let publishers = PendingPublishers::new(1);
    let slot = publishers.reserve().await;
    assert_eq!(publishers.available(), 0);

    let session =
        tokio::spawn(async move { run_session(pending, &services, &config(), slot).await });
    let live = await_published(&harness.store).await;

    assert_eq!(
        publishers.available(),
        1,
        "a running publisher must not hold admission capacity: the budget \
         exists to absorb connection bursts, not to shadow the session cap"
    );

    harness.sessions.stop_all(StopReason::Cancelled);
    tokio::time::timeout(Duration::from_secs(5), session)
        .await
        .expect("the session stops")
        .expect("the session task succeeds")
        .expect("cancellation is not a failure");
    assert!(live.is_ended());
}

#[tokio::test]
async fn a_cancelled_session_still_leaves_a_playable_stream() {
    let harness = Harness::healthy();
    let services = harness.services.clone();
    let pending = harness.pending(Ending::Stall);

    let session = tokio::spawn(async move {
        run_session(pending, &services, &config(), PendingPermit::unlimited()).await
    });

    // Wait for the pipeline to reach the live loop and publish its pre-roll.
    let live = await_published(&harness.store).await;
    assert!(
        live.snapshot().renditions[0]
            .snapshot()
            .open_segment
            .as_ref()
            .is_some_and(|segment| segment.parts.len() >= 3)
    );

    harness.sessions.stop_all(StopReason::Cancelled);

    let outcome = tokio::time::timeout(Duration::from_secs(5), session)
        .await
        .expect("the session stops")
        .expect("the session task succeeds")
        .expect("cancellation is not a failure");

    assert_eq!(outcome, SessionOutcome::Cancelled);
    assert!(
        live.is_ended(),
        "an operator cancellation ends the stream: nothing is coming to continue it"
    );
    // Draining closed the open segment even though the publisher was stopped.
    assert_eq!(harness.finish_reasons(), [FinishReason::Final]);
    assert_eq!(harness.meters.snapshot().segments_published, 1);
    assert_eq!(harness.recorder.names().last(), Some(&"ended"));
    assert_eq!(harness.store.leased(), 0);
}

#[tokio::test]
async fn a_second_publisher_takes_the_stream_over() {
    let harness = Harness::healthy();
    let incumbent_services = harness.services.clone();
    let incumbent_pending = harness.pending(Ending::Stall);

    let incumbent = tokio::spawn(async move {
        run_session(
            incumbent_pending,
            &incumbent_services,
            &config(),
            PendingPermit::unlimited(),
        )
        .await
    });
    await_published(&harness.store).await;

    let takeover_services = harness.services.clone();
    let takeover_pending = harness.pending(Ending::Stall);
    let takeover = tokio::spawn(async move {
        run_session(
            takeover_pending,
            &takeover_services,
            &config(),
            PendingPermit::unlimited(),
        )
        .await
    });

    let outcome = tokio::time::timeout(Duration::from_secs(5), incumbent)
        .await
        .expect("the incumbent stops")
        .expect("the incumbent task succeeds")
        .expect("being replaced is not a failure");
    assert_eq!(outcome, SessionOutcome::Replaced);
    assert_eq!(harness.meters.snapshot().sessions_replaced, 1);

    // The incumbent relinquished rather than closed: telling viewers the stream
    // ended would cost them a reconnect the successor does not need.
    assert_eq!(harness.finish_reasons(), [FinishReason::Superseded]);
    let live = harness.live();
    assert!(
        !live.is_ended(),
        "a taken-over stream is not an ended stream"
    );
    assert_eq!(
        harness.store.leased(),
        1,
        "the successor held the stream throughout the handover"
    );

    await_published(&harness.store).await;
    harness.sessions.stop_all(StopReason::Cancelled);
    tokio::time::timeout(Duration::from_secs(5), takeover)
        .await
        .expect("the takeover stops")
        .expect("the takeover task succeeds")
        .expect("cancellation is not a failure");
}

#[tokio::test]
async fn a_stream_policy_can_reject_takeovers_before_transport_acceptance() {
    let harness = Harness::new(
        None,
        StreamPolicy {
            takeovers: TakeoverPolicy::Deny,
            ..StreamPolicy::permissive()
        },
        Faults::default(),
    );
    let incumbent_services = harness.services.clone();
    let incumbent_pending = harness.pending(Ending::Stall);
    let incumbent = tokio::spawn(async move {
        run_session(
            incumbent_pending,
            &incumbent_services,
            &config(),
            PendingPermit::unlimited(),
        )
        .await
    });
    await_published(&harness.store).await;

    let error = harness
        .run(Ending::Eof)
        .await
        .expect_err("the incumbent is protected");

    assert_eq!(
        error,
        SessionError::Registry(RegistryError::AlreadyPublished { stream: stream() })
    );
    assert_eq!(
        harness
            .calls()
            .into_iter()
            .filter(|call| *call == "accept")
            .count(),
        1,
        "only the incumbent crossed the transport boundary"
    );
    assert_eq!(harness.sessions.len(), 1);

    harness.sessions.stop_all(StopReason::Cancelled);
    tokio::time::timeout(Duration::from_secs(5), incumbent)
        .await
        .expect("the incumbent stops")
        .expect("the incumbent task succeeds")
        .expect("cancellation is not a failure");
}

#[tokio::test]
async fn a_stalled_publisher_is_terminated_as_unhealthy() {
    let harness = Harness::healthy();
    let services = harness.services.clone();
    let pending = harness.pending(Ending::Stall);
    let config = SessionConfig {
        supervision: SupervisionPolicy {
            health: HealthPolicy {
                source_stall_timeout: Duration::from_millis(50),
                media_stall_timeout: Duration::from_millis(50),
                stalled_publication_multiplier: 1,
                minimum_publication_stall_tolerance: Duration::from_millis(50),
            },
            health_interval: Duration::from_millis(10),
        },
        ..config()
    };

    let error = tokio::time::timeout(
        Duration::from_secs(5),
        run_session(pending, &services, &config, PendingPermit::unlimited()),
    )
    .await
    .expect("supervision gives up")
    .expect_err("a stalled publisher fails its session");

    assert!(matches!(
        error,
        SessionError::Supervision(SupervisionError::Unhealthy(_))
    ));
    assert_eq!(harness.meters.snapshot().unhealthy_terminations, 1);
    assert!(harness.recorder.names().contains(&"unhealthy"));
    assert_eq!(harness.store.leased(), 0);
}

#[tokio::test]
async fn a_session_that_never_publishes_is_terminated_instead_of_running_forever() {
    // A defective muxer accepts every sample and emits nothing. Source and
    // media keep marking liveness, so the session has no stall to trip —
    // only the missing first publication can end it. The health check must
    // escalate that on the muxer's promised cadence rather than wait forever.
    let harness = Harness::faulty(Faults {
        muxer_swallows_samples: true,
        ..Faults::default()
    });
    let services = harness.services.clone();
    let pending = harness.pending(Ending::Endless);
    let config = SessionConfig {
        supervision: SupervisionPolicy {
            health: HealthPolicy {
                // Long enough that only the publication deadline can fire:
                // the source and media marks stay fresh by construction.
                source_stall_timeout: Duration::from_hours(1),
                media_stall_timeout: Duration::from_hours(1),
                stalled_publication_multiplier: 3,
                minimum_publication_stall_tolerance: Duration::ZERO,
            },
            health_interval: Duration::from_millis(10),
        },
        ..config()
    };

    // The muxer promises a one-second cadence, so the first publication is
    // owed within three seconds of Running. Outliving that without one is a
    // stall; anything else is a session parked forever.
    let error = tokio::time::timeout(
        Duration::from_secs(10),
        run_session(pending, &services, &config, PendingPermit::unlimited()),
    )
    .await
    .expect("the never-publishing session is terminated by supervision")
    .expect_err("a session that never publishes is unhealthy");

    assert!(matches!(
        error,
        SessionError::Supervision(SupervisionError::Unhealthy(
            HealthEvaluation::PublicationStalled { .. }
        ))
    ));
    assert_eq!(harness.meters.snapshot().unhealthy_terminations, 1);
    assert_eq!(harness.store.leased(), 0);
}

#[tokio::test]
async fn the_registry_reports_the_phase_a_session_is_in() {
    let harness = Harness::healthy();
    let services = harness.services.clone();
    let pending = harness.pending(Ending::Stall);

    let session = tokio::spawn(async move {
        run_session(pending, &services, &config(), PendingPermit::unlimited()).await
    });
    await_published(&harness.store).await;

    let snapshots = harness.sessions.snapshot();
    assert_eq!(snapshots.len(), 1);
    assert_eq!(snapshots[0].stream, stream());
    assert_eq!(snapshots[0].phase, super::Phase::Running);
    assert_eq!(
        snapshots[0].tracks,
        TrackCounts {
            audio: 0,
            subtitle: 0,
            video: 1,
        }
    );

    harness.sessions.stop_all(StopReason::Cancelled);
    tokio::time::timeout(Duration::from_secs(5), session)
        .await
        .expect("the session stops")
        .expect("the session task succeeds")
        .expect("cancellation is not a failure");
}

#[tokio::test]
async fn ending_reports_a_completed_session() {
    let harness = Harness::healthy();
    harness.run(Ending::Eof).await.expect("session succeeds");

    let ended = harness
        .recorder
        .events
        .lock()
        .iter()
        .filter_map(|event| match event {
            SessionEvent::Ended { end } => Some(*end),
            _ => None,
        })
        .next_back();
    assert_eq!(ended, Some(SessionEnd::Ended));
}

#[tokio::test]
async fn cancelling_still_flushes_media_the_normalizer_was_holding_back() {
    let harness = Harness::faulty(Faults {
        normalizer_reorders: true,
        ..Faults::default()
    });
    let services = harness.services.clone();
    let pending = harness.pending(Ending::Stall);

    let session = tokio::spawn(async move {
        run_session(pending, &services, &config(), PendingPermit::unlimited()).await
    });
    await_published(&harness.store).await;
    harness.sessions.stop_all(StopReason::Cancelled);

    tokio::time::timeout(Duration::from_secs(5), session)
        .await
        .expect("the session stops")
        .expect("the session task succeeds")
        .expect("cancellation is not a failure");

    // Four packets were accepted; the reorder buffer was holding the last one
    // when the stop arrived. A drain that only closed the muxer would publish
    // three parts and silently lose the fourth.
    assert_eq!(harness.meters.snapshot().parts_published, 4);
}

#[tokio::test]
async fn a_muxer_that_cannot_finalize_is_reported_without_failing_the_session() {
    let harness = Harness::faulty(Faults {
        muxer_finish_fails: true,
        ..Faults::default()
    });

    let outcome = harness
        .run(Ending::Eof)
        .await
        .expect("a failed flush does not retract a session that ran");

    assert_eq!(outcome, SessionOutcome::Ended);
    let totals = harness.meters.snapshot();
    assert_eq!(totals.sessions_completed, 1);
    assert_eq!(totals.sessions_failed, 0);
    assert_eq!(totals.drain_failures, 1);
    // The four parts published before the flush are still delivered; only the
    // closing segment is lost.
    assert_eq!(totals.parts_published, 4);
    assert_eq!(totals.segments_published, 0);
    assert_eq!(
        harness.recorder.names(),
        [
            "accepted",
            "tracks_discovered",
            "timeline_calibrated",
            "segmentation_locked",
            "running",
            "draining",
            "drain_failed",
            "ended",
        ]
    );
}

#[tokio::test]
async fn a_batch_larger_than_the_input_limit_ends_the_session() {
    let harness = Harness::healthy();
    let config = SessionConfig {
        input: InputLimits {
            maximum_packets_per_batch: 2,
            ..InputLimits::permissive()
        },
        ..config()
    };

    let error = harness
        .run_with(Ending::Eof, &config)
        .await
        .expect_err("an oversized batch is rejected");

    assert!(
        matches!(
            &error,
            SessionError::Preroll(crate::segment::PrerollError::Media(
                crate::media::MediaError::Limit(LimitError::BatchTooLarge {
                    unit: BatchUnit::Packets,
                    limit: 2,
                    found: 3,
                })
            ))
        ),
        "unexpected error: {error}"
    );
    assert_eq!(harness.meters.snapshot().sessions_failed, 1);
}

#[tokio::test(start_paused = true)]
async fn excessive_packet_density_ends_the_session() {
    let harness = Harness::healthy();
    let config = SessionConfig {
        input: InputLimits {
            maximum_packets_per_media_second: 2,
            ..InputLimits::permissive()
        },
        ..config()
    };

    let error = harness
        .run_with(Ending::Eof, &config)
        .await
        .expect_err("media above its density limit is rejected");

    assert!(
        matches!(
            &error,
            SessionError::Preroll(crate::segment::PrerollError::Media(
                crate::media::MediaError::Limit(LimitError::MediaDensityExceeded {
                    unit: DensityUnit::Packets,
                    limit: 2,
                    ..
                })
            ))
        ),
        "unexpected error: {error}"
    );
}

#[tokio::test(start_paused = true)]
async fn a_handshake_that_never_completes_does_not_hold_a_task_forever() {
    let harness = Harness::healthy();
    let config = SessionConfig {
        maximum_admission_time: Duration::from_secs(3),
        ..config()
    };

    let error = run_session(
        Box::new(SilentPending),
        &harness.services,
        &config,
        PendingPermit::unlimited(),
    )
    .await
    .expect_err("a silent peer is timed out");

    assert_eq!(
        error,
        SessionError::TimedOut {
            phase: super::Phase::Accepted
        }
    );
}

#[tokio::test(start_paused = true)]
async fn a_source_that_hangs_while_probing_does_not_hold_a_task_forever() {
    let harness = Harness::healthy();
    let config = SessionConfig {
        discovery: DiscoveryLimits {
            maximum_wall_time: Duration::from_secs(3),
            ..config().discovery
        },
        ..config()
    };
    let pending = harness.pending_for("camera", Ending::HangOnDiscovery);

    let error = run_session(
        pending,
        &harness.services,
        &config,
        PendingPermit::unlimited(),
    )
    .await
    .expect_err("a hanging probe is timed out");

    assert_eq!(
        error,
        SessionError::TimedOut {
            phase: super::Phase::Discovering
        }
    );
    assert_eq!(harness.meters.snapshot().sessions_failed, 1);
}

#[tokio::test]
async fn a_node_at_capacity_turns_a_new_publisher_away_in_the_protocol() {
    let harness = Harness::healthy();
    let sessions = Registry::with_capacity(1);
    let services = Services {
        sessions: sessions.clone(),
        ..harness.services.clone()
    };
    let held = services.clone();
    let pending = harness.pending(Ending::Stall);
    let incumbent = tokio::spawn(async move {
        run_session(pending, &held, &config(), PendingPermit::unlimited()).await
    });
    await_published(&harness.store).await;

    // A different stream, so this is a genuinely new session rather than a
    // takeover of the one already running.
    let overflow = harness.pending_for("other", Ending::Eof);
    let error = run_session(overflow, &services, &config(), PendingPermit::unlimited())
        .await
        .expect_err("a full node refuses the publication");

    assert_eq!(error, SessionError::Capacity(AtCapacity { maximum: 1 }));
    assert!(
        harness.calls().contains(&"reject"),
        "the publisher is told, not silently dropped"
    );
    assert_eq!(harness.meters.snapshot().publishers_rejected, 1);

    sessions.stop_all(StopReason::Cancelled);
    tokio::time::timeout(Duration::from_secs(5), incumbent)
        .await
        .expect("the incumbent stops")
        .expect("the incumbent task succeeds")
        .expect("cancellation is not a failure");
}

/// A peer that completes its handshake and then says nothing.
struct SilentPending;

impl PendingPublish for SilentPending {
    fn publish_request(&self) -> Result<PublishRequest, TransportError> {
        Ok(PublishRequest {
            protocol: IngestProtocol::Rtmp,
            resource: PublishResource {
                namespace: Some("live".into()),
                name: "camera".into(),
            },
            credential: PresentedCredential::new("secret"),
            client: ClientInfo {
                remote_address: "127.0.0.1:1935".parse().expect("constant address is valid"),
                encoder: None,
                protocol_version: None,
            },
        })
    }

    fn accept(
        self: Box<Self>,
        _grant: PublishGrant,
        _meters: Arc<dyn SourceMeters>,
    ) -> BoxFuture<'static, Result<AcceptedPublish, TransportError>> {
        Box::pin(std::future::pending())
    }

    fn reject(
        self: Box<Self>,
        _rejection: PublishRejection,
    ) -> BoxFuture<'static, Result<(), TransportError>> {
        Box::pin(std::future::pending())
    }
}

/// Waits until a session has published its first media into the store.
async fn await_published(store: &StreamStore) -> Arc<crate::delivery::hls::LiveStream> {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let Some(live) = store.get(&stream())
                && live.snapshot().renditions.iter().any(|rendition| {
                    let media = rendition.snapshot();
                    media.open_segment.is_some() || media.has_completed_segment()
                })
            {
                return live;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("the session publishes")
}

#[tokio::test]
async fn a_source_failure_after_media_was_published_fails_the_session_but_keeps_the_stream_resumable()
 {
    let harness = Harness::healthy();

    let outcome = harness.run(Ending::FailAfter(1)).await;

    assert!(matches!(
        outcome,
        Err(SessionError::Supervision(SupervisionError::Execution(
            ExecutionError::Media(_)
        )))
    ));
    assert_eq!(harness.meters.snapshot().sessions_failed, 1);
    assert_eq!(harness.recorder.names().last(), Some(&"failed"));
    // The pre-roll replay was published before the failing live-loop fill.
    assert_eq!(harness.meters.snapshot().parts_published, 3);
    assert_eq!(
        harness.finish_reasons(),
        [],
        "a failed session skips the drain entirely"
    );
    assert!(
        !harness.live().is_ended(),
        "a failed publisher may reconnect within the idle window"
    );
    assert_eq!(harness.store.leased(), 0);
}

#[tokio::test]
async fn a_source_that_lied_about_eof_is_never_read_again() {
    let harness = Harness::healthy();

    let outcome = harness
        .run(Ending::LyingEof)
        .await
        .expect("a lying EOF still ends cleanly");

    assert_eq!(outcome, SessionOutcome::Ended);
    assert_eq!(
        harness
            .calls()
            .into_iter()
            .filter(|call| *call == "fill")
            .count(),
        1,
        "the hidden batch is never read once the source reported end of input"
    );
    assert_eq!(harness.meters.snapshot().parts_published, 3);
    assert_eq!(harness.meters.snapshot().segments_published, 1);
}

#[tokio::test]
async fn a_muxer_failure_after_parts_were_published_keeps_those_parts_fetchable() {
    let harness = Harness::faulty(Faults {
        muxer_push_fails_after: Some(3),
        ..Faults::default()
    });

    let outcome = harness.run(Ending::Eof).await;

    assert!(matches!(
        outcome,
        Err(SessionError::Supervision(SupervisionError::Execution(
            ExecutionError::Mux(_)
        )))
    ));
    assert_eq!(harness.meters.snapshot().sessions_failed, 1);
    assert_eq!(harness.meters.snapshot().parts_published, 3);
    // The parts that made it out before the failure remain fetchable, and the
    // open segment is left exactly as it was.
    let rendition = harness
        .live()
        .rendition(RenditionId(0))
        .expect("the rendition exists");
    let open = rendition
        .open_segment
        .as_ref()
        .expect("the open segment survives the failed push");
    assert_eq!(open.parts.len(), 3);
    assert!(!harness.live().is_ended());
}

#[tokio::test]
async fn a_muxer_that_skips_a_chunk_index_is_refused_before_it_can_corrupt_the_segment() {
    let harness = Harness::faulty(Faults {
        muxer_skips_chunk_index: true,
        ..Faults::default()
    });

    let outcome = harness.run(Ending::Eof).await;

    assert!(matches!(
        outcome,
        Err(SessionError::Supervision(SupervisionError::Execution(
            ExecutionError::Delivery(_)
        )))
    ));
    assert_eq!(
        harness.meters.snapshot().parts_published,
        1,
        "the mis-indexed chunk and everything after it is refused wholesale"
    );
    let rendition = harness
        .live()
        .rendition(RenditionId(0))
        .expect("the rendition exists");
    let open = rendition
        .open_segment
        .as_ref()
        .expect("the open segment survives the refusal");
    assert_eq!(open.parts.len(), 1);
    assert!(!harness.live().is_ended());
}

#[tokio::test]
async fn a_publisher_capacity_failure_fails_the_session_but_not_the_stream() {
    let harness = Harness::faulty(Faults {
        // The initialization and the first two chunks are written (writes
        // zero, one, and two); the third chunk's write is refused.
        publisher_fails_after: Some(3),
        ..Faults::default()
    });

    let outcome = harness.run(Ending::Eof).await;

    assert!(matches!(
        outcome,
        Err(SessionError::Supervision(SupervisionError::Execution(
            ExecutionError::Delivery(_)
        )))
    ));
    assert_eq!(harness.meters.snapshot().sessions_failed, 1);
    assert_eq!(
        harness.meters.snapshot().parts_published,
        2,
        "the writes before the failing one were delivered"
    );
    assert!(!harness.live().is_ended());
    assert_eq!(harness.store.leased(), 0);
}

#[tokio::test]
async fn a_panicking_source_unwinds_registry_and_lease() {
    let harness = Harness::faulty(Faults {
        source_panics_after: Some(1),
        ..Faults::default()
    });
    let services = harness.services.clone();
    let pending = harness.pending(Ending::Stall);

    let session = tokio::spawn(async move {
        run_session(pending, &services, &config(), PendingPermit::unlimited()).await
    });
    let joined = tokio::time::timeout(Duration::from_secs(5), session)
        .await
        .expect("the panicking session finishes unwinding")
        .expect_err("the injected panic escapes the session task");

    assert!(joined.is_panic(), "the panic is not caught by the pipeline");
    assert!(
        harness.sessions.is_empty(),
        "unwinding removes the registry entry"
    );
    assert_eq!(harness.store.leased(), 0, "unwinding releases the lease");
    assert!(
        !harness.live().is_ended(),
        "an unplanned crash leaves the stream resumable"
    );
    assert_eq!(harness.meters.snapshot().sessions_failed, 0);
}

#[tokio::test]
async fn a_reconnect_storm_keeps_one_lease_and_one_durable_rendition() {
    const STORM: u32 = 5;
    let harness = Harness::healthy();

    let mut incumbent: Option<tokio::task::JoinHandle<Result<SessionOutcome, SessionError>>> = None;
    for cycle in 0..STORM {
        let services = harness.services.clone();
        let pending = harness.pending(Ending::Stall);
        let session = tokio::spawn(async move {
            run_session(pending, &services, &config(), PendingPermit::unlimited()).await
        });
        await_published(&harness.store).await;

        if let Some(previous) = incumbent.take() {
            let outcome = tokio::time::timeout(Duration::from_secs(5), previous)
                .await
                .expect("the displaced session stops")
                .expect("the displaced session task succeeds")
                .expect("displacement is not a failure");
            assert_eq!(outcome, SessionOutcome::Replaced);
            assert_eq!(
                harness.meters.snapshot().sessions_replaced,
                u64::from(cycle),
                "each cycle displaces exactly the previous publisher"
            );
        }

        let live = harness.live();
        assert_eq!(
            live.snapshot().renditions.len(),
            1,
            "the storm never forks the durable rendition"
        );
        assert_eq!(
            harness.store.leased(),
            1,
            "a successor holds the lease continuously across the storm"
        );
        assert!(!live.is_ended(), "the stream never ends mid-storm");
        incumbent = Some(session);
    }

    harness.sessions.stop_all(StopReason::Cancelled);
    let final_outcome = tokio::time::timeout(
        Duration::from_secs(5),
        incumbent.expect("the storm has a survivor"),
    )
    .await
    .expect("the survivor stops")
    .expect("the survivor task succeeds")
    .expect("cancellation is not a failure");
    assert_eq!(final_outcome, SessionOutcome::Cancelled);
    assert!(harness.live().is_ended());
    assert_eq!(harness.store.leased(), 0);
}

#[tokio::test]
async fn concurrent_takeovers_resolve_to_exactly_one_survivor() {
    let harness = Harness::healthy();
    let mut sessions = Vec::new();
    for _ in 0..3 {
        let services = harness.services.clone();
        let pending = harness.pending(Ending::Stall);
        sessions.push(tokio::spawn(async move {
            run_session(pending, &services, &config(), PendingPermit::unlimited()).await
        }));
    }

    // Registration is serialized by the registry lock, and each registration
    // displaces the current incumbent, so however the three interleave,
    // exactly one session survives. The losers are either displaced by a
    // later registration or refused while an incumbent is still draining.
    tokio::time::timeout(Duration::from_secs(5), async {
        while harness.sessions.len() != 1 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("exactly one session survives the scramble");
    assert_eq!(harness.store.leased(), 1);

    harness.sessions.stop_all(StopReason::Cancelled);
    let mut outcomes = Vec::new();
    for session in sessions {
        outcomes.push(
            tokio::time::timeout(Duration::from_secs(5), session)
                .await
                .expect("every session resolves")
                .expect("no session task panics"),
        );
    }

    assert_eq!(
        outcomes
            .iter()
            .filter(|outcome| matches!(outcome, Ok(SessionOutcome::Cancelled)))
            .count(),
        1,
        "exactly one survivor is left to be cancelled"
    );
    assert_eq!(
        outcomes
            .iter()
            .filter(|outcome| {
                matches!(
                    outcome,
                    Ok(SessionOutcome::Replaced)
                        | Err(SessionError::Registry(
                            RegistryError::TakeoverInProgress { .. }
                        ))
                )
            })
            .count(),
        2,
        "the two losers were displaced or refused, never leaked"
    );
    assert_eq!(harness.store.leased(), 0);
}
