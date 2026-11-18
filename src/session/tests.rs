use std::{collections::VecDeque, sync::Arc, time::Duration};

use parking_lot::Mutex;
use tokio::task::JoinHandle;

use crate::{
    admission::{
        AdmissionError, Authenticator, ClientInfo, IngestProtocol, PresentedCredential, Principal,
        PublishGrant, PublishRequest, PublishResource, StreamPolicy, TakeoverPolicy,
    },
    delivery::hls::{
        HlsError, HlsPublisher, LiveStream, PublishOutcome, PublisherFactory,
        StorePublisherFactory, StreamStore,
    },
    delivery::store::StoreWriteError,
    domain::{
        Appender, BoxFuture, Codec, InputMode, MediaKind, Payload, RenditionId, SessionId,
        StreamId, Timebase, TrackCounts, TrackId,
    },
    media::{
        MediaNormalizer, NormalizeError, NormalizedMedia, PresentationPlan, StartedNormalizer,
        TimelineCalibration, VideoSample, start_normalizer,
    },
    mux::{
        FinishReason, InitializationSegment, MuxError, Muxer, MuxerStartRequest, PackagedChunk,
        PackagedMedia, PackagedPresentation, PackagedSegmentCompletion, PackagingRenditionId,
        PackagingSegmentId, StartedMuxer, fixtures as mux_fixtures, fixtures::RenditionBuilder,
    },
    observe::{
        EventObserver, Events, ProcessMeters, SessionEnd, SessionEvent, SourceMeters,
        lifecycle::{Event, Kind, Outcome, Projector, PublisherEnded},
    },
    segment::{PrerollLimits, SegmentationPolicy},
    source::{
        AcceptedPublish, BatchUnit, DensityUnit, DiscoveryLimits, DiscoveryReport, InputLimits,
        InputState, LimitError, Packet, PacketSource, PendingPublish, PublishRejection,
        SourceError, TransportError,
    },
};

use super::{
    AtCapacity, ExecutionError, HealthPolicy, PendingPermit, PendingPublishers, Registry,
    RegistryError, Services, SessionConfig, SessionError, SessionOutcome, StopReason,
    SupervisionError, SupervisionPolicy, run_session,
};

const SECOND: i64 = 90_000;
const WAIT_LIMIT: Duration = Duration::from_secs(5);

type CallLog = Arc<Mutex<Vec<&'static str>>>;
type FinishLog = Arc<Mutex<Vec<FinishReason>>>;
type SessionTask = JoinHandle<Result<SessionOutcome, SessionError>>;
/// Captures the session's pipeline budget so tests can check it drains.
type BudgetProbe = Arc<Mutex<Option<crate::domain::PipelineBudget>>>;

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
    YieldDuringDiscovery,
    /// Fails with a source error once this many batches were delivered.
    FailAfter(u32),
    /// Reports a clean end of input while still holding media back.
    LyingEof,
}

#[derive(Clone, Copy)]
enum ScriptedTrack {
    Video,
    Audio,
}

struct FakeSource {
    log: CallLog,
    meters: Arc<dyn SourceMeters>,
    track: ScriptedTrack,
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
        let track = self.track;
        Box::pin(async move {
            if ending == Ending::YieldDuringDiscovery {
                tokio::task::yield_now().await;
            }
            if ending == Ending::HangOnDiscovery {
                std::future::pending::<()>().await;
            }
            let track = match track {
                ScriptedTrack::Video => {
                    let mut track = crate::domain::fixtures::track(0, MediaKind::Video);
                    if let crate::domain::MediaParameters::Video { frame_rate, .. } =
                        &mut track.parameters
                    {
                        *frame_rate = Some(crate::domain::FrameRate::new(nz::u32!(1), nz::u32!(1)));
                    }
                    track
                }
                ScriptedTrack::Audio => {
                    crate::domain::fixtures::TrackBuilder::new(0, MediaKind::Audio)
                        .timebase(Timebase::new(nz::u32!(1), nz::u32!(48_000)))
                        .codec_extradata(&[0x11, 0x90][..])
                        .build()
                }
            };
            Ok(DiscoveryReport {
                tracks: crate::domain::fixtures::catalog(vec![track]),
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
            if let Some(batch) = self.batches.pop_front() {
                let bytes = batch.iter().map(|packet| packet.payload.len() as u64).sum();
                let packets = batch.len() as u64;
                for packet in batch {
                    out.push(packet);
                }
                self.meters.source_progress(bytes, packets);
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
                Ending::Eof
                | Ending::HangOnDiscovery
                | Ending::YieldDuringDiscovery
                | Ending::LyingEof => Ok(InputState::Closed),
                Ending::Interrupted => Ok(InputState::Interrupted),
                Ending::Stall => std::future::pending().await,
                Ending::CodecChange => {
                    self.meters.codec_parameters_changed();
                    Err(SourceError::CodecParametersChanged {
                        track_id: TrackId(0),
                    })
                }
                Ending::FailAfter(_) => Err(SourceError::Input("injected source failure".into())),
            }
        })
    }
}

struct FakePending {
    log: CallLog,
    resource: &'static str,
    credential: &'static str,
    track: ScriptedTrack,
    batches: Vec<Vec<Packet>>,
    ending: Ending,
    panics_after: Option<u32>,
    budget_probe: Option<BudgetProbe>,
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
            credential: PresentedCredential::new(self.credential),
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
        if let Some(probe) = &self.budget_probe {
            *probe.lock() = meters.pipeline_budget().cloned();
        }
        Box::pin(async move {
            Ok(AcceptedPublish {
                source: Box::new(FakeSource {
                    log: self.log.clone(),
                    meters,
                    track: self.track,
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

type StartNormalizer = fn(
    &PresentationPlan,
    &TimelineCalibration,
    InputMode,
) -> Result<StartedNormalizer, NormalizeError>;

/// Scripted stage startup for lifecycle tests, with an optional real normalizer
/// for cases that exercise timestamp handling through the session.
#[derive(Clone)]
pub(crate) struct StageOverrides {
    on_mux_start: Option<Arc<dyn Fn() + Send + Sync>>,
    normalizer: Option<StartNormalizer>,
    log: CallLog,
    finished: FinishLog,
    faults: Faults,
}

impl StageOverrides {
    pub(super) fn start_normalizer(
        &self,
        presentation: &PresentationPlan,
        timeline: &TimelineCalibration,
        mode: InputMode,
    ) -> Result<StartedNormalizer, NormalizeError> {
        if let Some(start) = self.normalizer {
            return start(presentation, timeline, mode);
        }
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

    pub(super) fn start_muxer(&self, request: MuxerStartRequest<'_>) -> StartedMuxer {
        record(&self.log, "muxer_start");
        if let Some(on_start) = &self.on_mux_start {
            on_start();
        }
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
        StartedMuxer {
            muxer: Box::new(FakeMuxer {
                initialized: false,
                chunks: 0,
                segment: 0,
                segment_start: 0,
                pushes: 0,
                duration: 0,
                finished: Arc::clone(&self.finished),
                faults: self.faults,
            }),
            presentation: Arc::new(presentation),
        }
    }
}

/// Optionally holds one sample back, standing in for a real reorder buffer.
struct FakeNormalizer {
    reorders: bool,
    held: Option<NormalizedMedia>,
}

impl MediaNormalizer for FakeNormalizer {
    fn push(
        &mut self,
        packet: Packet,
        out: &mut dyn Appender<NormalizedMedia>,
    ) -> Result<(), NormalizeError> {
        let pts = packet
            .pts
            .ok_or_else(|| NormalizeError::Processing("test packet has no PTS".into()))?;
        let sample = NormalizedMedia::Video(VideoSample {
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

    fn finish(&mut self, out: &mut dyn Appender<NormalizedMedia>) -> Result<(), NormalizeError> {
        if let Some(held) = self.held.take() {
            out.push(held);
        }
        Ok(())
    }
}

/// Emits one part per sample; gaps close the current segment before absence.
struct FakeMuxer {
    initialized: bool,
    chunks: u32,
    segment: u64,
    segment_start: i64,
    pushes: u32,
    duration: u64,
    finished: FinishLog,
    faults: Faults,
}

impl Muxer for FakeMuxer {
    fn push(
        &mut self,
        sample: NormalizedMedia,
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
        if let NormalizedMedia::Gap(gap) = sample {
            if self.chunks > 0 {
                out.push(PackagedMedia::SegmentCompleted(PackagedSegmentCompletion {
                    rendition_id: PackagingRenditionId(0),
                    packaging_segment_id: PackagingSegmentId(self.segment),
                    media_start: self.segment_start,
                    duration: self.duration,
                }));
                self.segment += 1;
            }
            out.push(PackagedMedia::Gap(crate::mux::PackagedGap {
                rendition_id: PackagingRenditionId(0),
                packaging_segment_id: PackagingSegmentId(self.segment),
                media_start: gap.start,
                duration: gap.end.abs_diff(gap.start),
                parts: vec![gap.end.abs_diff(gap.start)],
            }));
            self.segment += 1;
            self.chunks = 0;
            self.duration = 0;
            return Ok(());
        }
        if self.chunks == 0 {
            self.segment_start = sample.pts();
        }
        out.push(PackagedMedia::Chunk(PackagedChunk {
            rendition_id: PackagingRenditionId(0),
            packaging_segment_id: PackagingSegmentId(self.segment),
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
        if self.chunks > 0 {
            out.push(PackagedMedia::SegmentCompleted(PackagedSegmentCompletion {
                rendition_id: PackagingRenditionId(0),
                packaging_segment_id: PackagingSegmentId(self.segment),
                media_start: self.segment_start,
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
    fn store(&self) -> &crate::delivery::store::StreamStore {
        self.inner.store()
    }

    fn start_if_current(
        &self,
        stream: &StreamId,
        presentation: Arc<PackagedPresentation>,
        permit: crate::delivery::store::PublicationPermit<'_>,
    ) -> Result<Box<dyn HlsPublisher>, HlsError> {
        let inner = self.inner.start_if_current(stream, presentation, permit)?;
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
    events: Mutex<Vec<(SessionId, SessionEvent)>>,
    streams: Mutex<Vec<crate::observe::StreamEvent>>,
}

impl EventObserver for Recorder {
    fn observe_stream(&self, _: StreamId, event: crate::observe::StreamEvent) {
        self.streams.lock().push(event);
    }

    fn observe(&self, session: SessionId, event: SessionEvent) {
        self.events.lock().push((session, event));
    }
}

impl Recorder {
    fn names(&self) -> Vec<&'static str> {
        self.events
            .lock()
            .iter()
            .map(|(_, event)| name(event))
            .collect()
    }

    fn last_end(&self) -> Option<SessionEnd> {
        self.events.lock().iter().rev().find_map(|(_, event)| {
            let SessionEvent::Ended { end } = event else {
                return None;
            };
            Some(*end)
        })
    }

    fn projected(&self) -> Vec<Event> {
        // Replay the full history so the projector can associate each end or
        // compensation notice with its original admission.
        let projector = Projector::new();
        self.events
            .lock()
            .iter()
            .filter_map(|(session, event)| projector.project(*session, event))
            .collect()
    }
}

fn publisher_end(event: &Event) -> Option<&PublisherEnded> {
    let Event::PublisherEnded(end) = event else {
        return None;
    };
    Some(end)
}

fn assert_event_count(events: &[Event], kind: Kind, expected: usize) {
    let count = events.iter().filter(|event| event.kind() == kind).count();
    assert_eq!(count, expected, "unexpected number of {kind} events");
}

fn name(event: &SessionEvent) -> &'static str {
    match event {
        SessionEvent::Compensation { .. } => "compensation",
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
        SessionEvent::PublisherBehindRealtime { .. } => "publisher_behind_realtime",
        SessionEvent::PublisherTrackingRealtime => "publisher_tracking_realtime",
        SessionEvent::Unhealthy { .. } => "unhealthy",
        SessionEvent::Draining => "draining",
        SessionEvent::DrainFailed { .. } => "drain_failed",
        SessionEvent::Ended { .. } => "ended",
        SessionEvent::Failed { .. } => "failed",
        SessionEvent::SegmentationContract { .. } => "contract",
    }
}

fn packet(pts: i64, random_access: bool) -> Packet {
    Packet {
        track_id: TrackId(0),
        pts: Some(pts),
        dts: Some(pts),
        duration: Some(SECOND),
        random_access: random_access || pts == 0,
        audio_trim: crate::domain::AudioTrim::default(),
        webvtt: crate::domain::WebVttCueMetadata::default(),
        subtitle_position: None,
        payload: Payload::from(vec![1, 2, 3]),
    }
}

fn audio_packet(pts: i64) -> Packet {
    Packet {
        track_id: TrackId(0),
        pts: Some(pts),
        dts: Some(pts),
        duration: Some(1024),
        random_access: false,
        audio_trim: crate::domain::AudioTrim::default(),
        webvtt: crate::domain::WebVttCueMetadata::default(),
        subtitle_position: None,
        payload: (&[0x21, 0x10, 0x04, 0x60, 0x8c, 0x1c][..]).into(),
    }
}

fn video_batches_before_issue(live: bool, issue_batch: Vec<Packet>) -> Vec<Vec<Packet>> {
    // The second batch reaches the live loop only when the first batch
    // supplies enough keyframes to lock segmentation during pre-roll.
    let frames = if live { 3 } else { 1 };
    let prefix = (0..frames)
        .map(|index| packet(index * SECOND, true))
        .collect();
    vec![prefix, issue_batch]
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
        memory_per_publisher: Some(crate::domain::PipelineBudget::DEFAULT_LIMIT),
        maximum_admission_time: Duration::from_secs(5),
        discovery: DiscoveryLimits {
            maximum_probe_bytes: 1_048_576,
            maximum_wall_time: Duration::from_secs(5),
        },
        input: InputLimits::permissive(),
        preroll: PrerollLimits {
            maximum_buffered_samples: 4_096,
            maximum_wall_time: Duration::from_secs(5),
            maximum_media_duration: Duration::from_secs(30),
        },
        // The synthetic source provides a one-second boundary before it may
        // stall. Lock that exact target so lifecycle tests reach publication
        // without waiting for evidence of a closer, later boundary.
        segmentation: SegmentationPolicy::latency_first(
            Duration::from_secs(1),
            Duration::from_secs(1),
        ),
        // Long enough that liveness never fires unless a test asks it to.
        supervision: SupervisionPolicy {
            health: HealthPolicy {
                stall: Duration::from_hours(1),
            },
            health_interval: Duration::from_hours(1),
        },
    }
}

struct Harness {
    services: Services,
    finished: FinishLog,
    log: CallLog,
    recorder: Arc<Recorder>,
    store: StreamStore,
    meters: ProcessMeters,
    panics_after: Option<u32>,
    budget_probe: BudgetProbe,
}

impl Harness {
    fn new(rejection: Option<AdmissionError>, policy: StreamPolicy, faults: Faults) -> Self {
        let log = CallLog::default();
        let finished = FinishLog::default();
        let recorder = Arc::new(Recorder::default());
        let events = Events::new(recorder.clone());
        let store = StreamStore::default().with_events(events.clone());
        let meters = ProcessMeters::default();
        let services = Services {
            stages: Some(StageOverrides {
                normalizer: None,
                on_mux_start: None,
                log: log.clone(),
                finished: finished.clone(),
                faults,
            }),
            authenticator: Arc::new(FakeAuthenticator {
                log: log.clone(),
                rejection,
                policy,
            }),
            publishers: Arc::new(FaultyPublisherFactory {
                inner: StorePublisherFactory::new(store.clone()).with_events(events.clone()),
                faults,
            }),
            sessions: Registry::default(),
            meters: meters.clone(),
            events,
        };

        Self {
            services,
            finished,
            log,
            recorder,
            store,
            meters,
            panics_after: faults.source_panics_after,
            budget_probe: BudgetProbe::default(),
        }
    }

    fn healthy() -> Self {
        Self::faulty(Faults::default())
    }

    fn faulty(faults: Faults) -> Self {
        Self::new(None, StreamPolicy::permissive(), faults)
    }

    fn with_normalizer(input_mode: InputMode, normalizer: StartNormalizer) -> Self {
        let policy = StreamPolicy {
            input_mode,
            ..StreamPolicy::permissive()
        };
        let mut harness = Self::new(None, policy, Faults::default());
        harness
            .services
            .stages
            .as_mut()
            .expect("scripted stages")
            .normalizer = Some(normalizer);
        harness
    }

    fn pending(&self, ending: Ending) -> Box<dyn PendingPublish> {
        self.pending_for("camera", ending)
    }

    fn pending_for(&self, resource: &'static str, ending: Ending) -> Box<dyn PendingPublish> {
        Box::new(FakePending {
            resource,
            panics_after: self.panics_after,
            budget_probe: Some(self.budget_probe.clone()),
            ..self.scripted_pending(scripted_batches(), ending)
        })
    }

    fn scripted_pending(&self, batches: Vec<Vec<Packet>>, ending: Ending) -> FakePending {
        FakePending {
            log: self.log.clone(),
            resource: "camera",
            credential: "secret",
            track: ScriptedTrack::Video,
            batches,
            ending,
            panics_after: None,
            budget_probe: None,
        }
    }

    async fn run(&self, ending: Ending) -> Result<SessionOutcome, SessionError> {
        self.run_with(ending, &config()).await
    }

    async fn run_with(
        &self,
        ending: Ending,
        config: &SessionConfig,
    ) -> Result<SessionOutcome, SessionError> {
        self.run_pending(self.pending(ending), config).await
    }

    async fn run_pending(
        &self,
        pending: Box<dyn PendingPublish>,
        config: &SessionConfig,
    ) -> Result<SessionOutcome, SessionError> {
        run_session(pending, &self.services, config, PendingPermit::unlimited()).await
    }

    async fn run_scripted_video(
        &self,
        batches: Vec<Vec<Packet>>,
    ) -> Result<SessionOutcome, SessionError> {
        let pending = self.scripted_pending(batches, Ending::Eof);
        self.run_pending(Box::new(pending), &config()).await
    }

    async fn run_scripted_audio(
        &self,
        batches: Vec<Vec<Packet>>,
    ) -> Result<SessionOutcome, SessionError> {
        // Exercise production muxing as well as normalization for audio.
        let pending = FakePending {
            track: ScriptedTrack::Audio,
            ..self.scripted_pending(batches, Ending::Eof)
        };
        let services = Services {
            stages: None,
            ..self.services.clone()
        };
        run_session(
            Box::new(pending),
            &services,
            &config(),
            PendingPermit::unlimited(),
        )
        .await
    }

    fn spawn_stalled(&self) -> SessionTask {
        self.spawn_pending(self.pending(Ending::Stall), PendingPermit::unlimited())
    }

    fn spawn_pending(&self, pending: Box<dyn PendingPublish>, slot: PendingPermit) -> SessionTask {
        let services = self.services.clone();
        tokio::spawn(async move { run_session(pending, &services, &config(), slot).await })
    }

    async fn cancel(&self, session: SessionTask, role: &str) -> SessionOutcome {
        self.services.sessions.stop_all(StopReason::Cancelled);
        join_session(session, role)
            .await
            .expect("cancellation is not a failure")
    }

    fn calls(&self) -> Vec<&'static str> {
        self.log.lock().clone()
    }

    fn finish_reasons(&self) -> Vec<FinishReason> {
        self.finished.lock().clone()
    }

    /// The stream this harness publishes, whether or not it is still leased.
    fn live(&self) -> Arc<LiveStream> {
        self.store.get(&stream()).expect("the stream is retained")
    }

    /// Waits until a session has published its first media into the store.
    async fn await_published(&self) -> Arc<LiveStream> {
        tokio::time::timeout(WAIT_LIMIT, async {
            loop {
                if let Some(live) = self.store.get(&stream())
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
}

async fn join_session(session: SessionTask, role: &str) -> Result<SessionOutcome, SessionError> {
    tokio::time::timeout(WAIT_LIMIT, session)
        .await
        .unwrap_or_else(|_| panic!("{role} did not finish within {WAIT_LIMIT:?}"))
        .unwrap_or_else(|error| panic!("{role} task failed to join: {error}"))
}

#[tokio::test(start_paused = true)]
async fn reconnect_cannot_reveal_a_retained_publisher_key() -> Result<(), SessionError> {
    for public_first in [false, true] {
        let harness = Harness::healthy();
        for (i, public) in [public_first, !public_first, true].into_iter().enumerate() {
            // Every grant names live/camera. Only the public requests have
            // a credential distinct from the playback route.
            let pending = FakePending {
                credential: if public { "separate-key" } else { "camera" },
                ..harness.scripted_pending(scripted_batches(), Ending::Interrupted)
            };
            let before = harness.recorder.streams.lock().len();
            harness.run_pending(Box::new(pending), &config()).await?;
            assert!(harness.live().is_playable());
            assert_eq!(harness.live().was_announced(), public_first);
            if i == 0 && public_first {
                let stream_events = harness.recorder.streams.lock();
                assert!(stream_events.contains(&crate::observe::StreamEvent::Available));
                assert!(
                    stream_events
                        .iter()
                        .any(|e| matches!(e, crate::observe::StreamEvent::SegmentReady(_)))
                );
            } else {
                assert_eq!(harness.recorder.streams.lock().len(), before);
            }
            let projected = harness.recorder.projected();
            assert_eq!(projected.len(), if public_first { 2 } else { 0 });
        }
        tokio::time::advance(Duration::from_hours(1)).await;
        assert_eq!(harness.store.maintain().retired, [(stream(), public_first)]);
        harness.run(Ending::Eof).await?;
        assert!(
            harness.live().was_announced(),
            "fresh stream may announce again"
        );
    }
    Ok(())
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
            "contract",
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
    assert!(harness.services.sessions.is_empty());
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
    assert_eq!(harness.recorder.last_end(), Some(SessionEnd::Interrupted));
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
            video: crate::admission::VideoAccept {
                codecs: crate::admission::Codecs::OneOf(vec![Codec::Av1]),
                ..crate::admission::VideoAccept::default()
            },
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
    assert!(harness.services.sessions.is_empty());
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
    assert!(harness.services.sessions.is_empty());
    assert_eq!(harness.store.leased(), 0);
}

#[tokio::test]
async fn an_admission_slot_is_returned_before_the_session_it_admitted_ends() {
    let harness = Harness::healthy();
    // One slot, so a session still holding it would make the budget observably
    // empty for as long as the publisher stays connected.
    let publishers = PendingPublishers::new(1);
    let slot = publishers.reserve().await;
    assert_eq!(publishers.available(), 0);

    let session = harness.spawn_pending(harness.pending(Ending::Stall), slot);
    let live = harness.await_published().await;

    assert_eq!(
        publishers.available(),
        1,
        "a running publisher must not hold admission capacity: the budget \
         exists to absorb connection bursts, not to shadow the session cap"
    );

    harness.cancel(session, "the session").await;
    assert!(live.is_ended());
}

#[tokio::test]
async fn a_cancelled_session_still_leaves_a_playable_stream() {
    let harness = Harness::healthy();
    let session = harness.spawn_stalled();

    // Wait for the pipeline to reach the live loop and publish its pre-roll.
    let live = harness.await_published().await;
    assert!(
        live.snapshot().renditions[0]
            .snapshot()
            .open_segment
            .as_ref()
            .is_some_and(|segment| segment.parts.len() >= 3)
    );

    let outcome = harness.cancel(session, "the session").await;

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
async fn late_discovery_cannot_take_the_successors_lease() -> Result<(), Box<dyn std::error::Error>>
{
    for (first_key, next_key) in [("camera", "rotated-key"), ("rotated-key", "camera")] {
        let harness = Harness::healthy();
        let limits = config();
        let pending = |credential, ending| -> Box<dyn PendingPublish> {
            Box::new(FakePending {
                credential,
                ..harness.scripted_pending(scripted_batches(), ending)
            })
        };
        let old = harness.run_pending(pending(first_key, Ending::YieldDuringDiscovery), &limits);
        tokio::pin!(old);
        tokio::select! {
            biased;
            result = &mut old => panic!("discovery should yield: {result:?}"),
            () = std::future::ready(()) => {}
        }
        assert_eq!(
            harness.services.sessions.snapshot()[0].phase,
            super::Phase::Discovering
        );

        assert!(
            harness.store.is_empty(),
            "no retained stream can supply the privacy flag"
        );
        let successor =
            harness.spawn_pending(pending(next_key, Ending::Stall), PendingPermit::unlimited());
        let live = harness.await_published().await;
        assert_eq!(old.await?, SessionOutcome::Replaced);
        assert_eq!(harness.store.leased(), 1);
        // Finalizing must write a new, fetchable segment through the successor's lease.
        assert_eq!(
            harness.cancel(successor, "the successor").await,
            SessionOutcome::Cancelled
        );
        assert!(live.is_ended());
        assert!(
            live.snapshot().renditions[0]
                .snapshot()
                .has_completed_segment()
        );
        assert!(harness.recorder.streams.lock().is_empty());
        // Only a formerly public predecessor owes a matching publisher-ended event.
        assert_eq!(
            harness.recorder.projected().len(),
            if first_key == "camera" { 0 } else { 2 }
        );
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn replacement_during_mux_start_preserves_the_successors_lease()
-> Result<(), Box<dyn std::error::Error>> {
    let harness = Harness::healthy();
    let mut predecessor = harness.services.clone();
    let at_mux = Arc::new(tokio::sync::Notify::new());
    let (resume, suspended) = std::sync::mpsc::channel();
    let reached = at_mux.clone();
    let suspended = Mutex::new(suspended);
    // Hold this startup poll after the outer stop check until the successor owns the lease.
    let stages = predecessor.stages.as_mut().expect("scripted stages");
    stages.on_mux_start = Some(Arc::new(move || {
        reached.notify_one();
        suspended
            .lock()
            .recv_timeout(WAIT_LIMIT)
            .expect("resume predecessor");
    }));
    let pending = harness.pending(Ending::Stall);
    let old = tokio::spawn(async move {
        run_session(pending, &predecessor, &config(), PendingPermit::unlimited()).await
    });

    tokio::time::timeout(WAIT_LIMIT, at_mux.notified()).await?;
    let successor = harness.spawn_stalled();
    let live = harness.await_published().await;
    resume.send(())?;
    assert_eq!(
        join_session(old, "the predecessor").await?,
        SessionOutcome::Replaced
    );
    assert_eq!(harness.store.leased(), 1);
    assert_eq!(harness.meters.snapshot().segments_published, 0);

    // Only the successor can flush its open segment after the predecessor has returned.
    assert_eq!(
        harness.cancel(successor, "the successor").await,
        SessionOutcome::Cancelled
    );
    assert_eq!(harness.meters.snapshot().segments_published, 1);
    assert!(
        live.snapshot().renditions[0]
            .snapshot()
            .has_completed_segment()
    );
    Ok(())
}

#[tokio::test]
async fn a_second_publisher_takes_the_stream_over() {
    let harness = Harness::healthy();
    let incumbent = harness.spawn_stalled();
    harness.await_published().await;

    let takeover = harness.spawn_stalled();

    let outcome = join_session(incumbent, "the incumbent")
        .await
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

    harness.await_published().await;
    harness.cancel(takeover, "the takeover").await;
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
    let incumbent = harness.spawn_stalled();
    harness.await_published().await;

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
    assert_eq!(harness.services.sessions.len(), 1);

    harness.cancel(incumbent, "the incumbent").await;
}

#[tokio::test]
async fn a_stalled_publisher_is_terminated_as_unhealthy() {
    let harness = Harness::healthy();
    let config = SessionConfig {
        supervision: SupervisionPolicy {
            health: HealthPolicy {
                stall: Duration::from_millis(50),
            },
            health_interval: Duration::from_millis(10),
        },
        ..config()
    };

    let error = tokio::time::timeout(
        Duration::from_secs(5),
        harness.run_with(Ending::Stall, &config),
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
async fn the_registry_reports_the_phase_a_session_is_in() {
    let harness = Harness::healthy();
    let session = harness.spawn_stalled();
    harness.await_published().await;

    let snapshots = harness.services.sessions.snapshot();
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

    harness.cancel(session, "the session").await;
}

#[tokio::test]
async fn ending_reports_a_completed_session() {
    let harness = Harness::healthy();
    harness.run(Ending::Eof).await.expect("session succeeds");

    assert_eq!(harness.recorder.last_end(), Some(SessionEnd::Ended));
}

#[tokio::test]
async fn cancelling_still_flushes_media_the_normalizer_was_holding_back() {
    let harness = Harness::faulty(Faults {
        normalizer_reorders: true,
        ..Faults::default()
    });
    let session = harness.spawn_stalled();
    harness.await_published().await;
    harness.cancel(session, "the session").await;

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
            "contract",
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
    let pending = harness.pending(Ending::HangOnDiscovery);

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
    let mut harness = Harness::healthy();
    harness.services.sessions = Registry::with_capacity(1);
    let incumbent = harness.spawn_stalled();
    harness.await_published().await;

    // A different stream, so this is a genuinely new session rather than a
    // takeover of the one already running.
    let overflow = harness.pending_for("other", Ending::Eof);
    let error = harness
        .run_pending(overflow, &config())
        .await
        .expect_err("a full node refuses the publication");

    assert_eq!(error, SessionError::Capacity(AtCapacity { maximum: 1 }));
    assert!(
        harness.calls().contains(&"reject"),
        "the publisher is told, not silently dropped"
    );
    assert_eq!(harness.meters.snapshot().publishers_rejected, 1);

    harness.cancel(incumbent, "the incumbent").await;
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
    let session = harness.spawn_stalled();
    let joined = tokio::time::timeout(WAIT_LIMIT, session)
        .await
        .expect("the panicking session finishes unwinding")
        .expect_err("the injected panic escapes the session task");

    assert!(joined.is_panic(), "the panic is not caught by the pipeline");
    assert!(
        harness.services.sessions.is_empty(),
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

    let mut incumbent = None;
    for cycle in 0..STORM {
        let session = harness.spawn_stalled();
        harness.await_published().await;

        if let Some(previous) = incumbent.take() {
            let outcome = join_session(previous, "the displaced session")
                .await
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

    let survivor = incumbent.expect("the storm has a survivor");
    let final_outcome = harness.cancel(survivor, "the survivor").await;
    assert_eq!(final_outcome, SessionOutcome::Cancelled);
    assert!(harness.live().is_ended());
    assert_eq!(harness.store.leased(), 0);
}

#[tokio::test]
async fn concurrent_takeovers_resolve_to_exactly_one_survivor() {
    let harness = Harness::healthy();
    let mut sessions = Vec::new();
    for _ in 0..3 {
        sessions.push(harness.spawn_stalled());
    }

    // Registration is serialized by the registry lock, and each registration
    // displaces the current incumbent, so however the three interleave,
    // exactly one session survives. The losers are either displaced by a
    // later registration or refused while an incumbent is still draining.
    tokio::time::timeout(WAIT_LIMIT, async {
        while harness.services.sessions.len() != 1 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("exactly one session survives the scramble");
    assert_eq!(harness.store.leased(), 1);

    harness.services.sessions.stop_all(StopReason::Cancelled);
    let mut outcomes = Vec::new();
    for session in sessions {
        outcomes.push(join_session(session, "a concurrent publisher").await);
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

#[tokio::test]
async fn backward_video_timestamps_fail_before_the_offending_sample_is_published()
-> Result<(), Box<dyn std::error::Error>> {
    // Exercise the production normalizer through the real session and store;
    // the test muxer publishes one part for every sample it receives.
    let harness = Harness::with_normalizer(InputMode::Permissive, start_normalizer);
    let batches = video_batches_before_issue(true, vec![packet(SECOND, false)]);
    let error = harness
        .run_scripted_video(batches)
        .await
        .expect_err("backward input must fail the live session");
    assert!(matches!(
        error,
        SessionError::Supervision(SupervisionError::Execution(ExecutionError::Media(
            crate::media::MediaError::Normalize(_)
        )))
    ));
    assert!(error.to_string().contains("video_timestamp_order"));
    let totals = harness.meters.snapshot();
    assert_eq!(totals.sessions_failed, 1);
    assert_eq!(
        totals.parts_published, 2,
        "only valid pre-roll samples reach the publisher"
    );
    assert_eq!(harness.recorder.names().last(), Some(&"failed"));
    assert!(
        harness.finish_reasons().is_empty(),
        "failure must not flush invalid media"
    );
    assert_eq!(harness.store.leased(), 0);
    assert!(
        !harness.live().is_ended(),
        "previous media remains available for reconnect"
    );
    Ok(())
}

#[tokio::test]
async fn timestamp_failures_report_once_during_preroll_and_live()
-> Result<(), Box<dyn std::error::Error>> {
    use crate::domain::TimestampIssueCode;
    for live in [false, true] {
        // This lifecycle fixture supplies one frame per second.
        let harness = Harness::with_normalizer(InputMode::Permissive, start_normalizer);
        let jump_to = if live { 4 * SECOND } else { 2 * SECOND };
        let batches = video_batches_before_issue(live, vec![packet(jump_to, true)]);
        let error = harness
            .run_scripted_video(batches)
            .await
            .expect_err("jump must fail");
        assert_eq!(
            super::timestamp_issue(&error)
                .expect("typed issue survives wrappers")
                .code,
            TimestampIssueCode::VideoTimestampJump
        );
        assert_eq!(
            harness.meters.timestamp_rejections(),
            vec![(("video_timestamp_jump".into(), "video".into()), 1)]
        );
        assert_eq!(
            harness.meters.snapshot().parts_published,
            if live { 2 } else { 0 }
        );
        assert_eq!(harness.store.leased(), 0);
        let events = harness.recorder.projected();
        assert_event_count(&events, Kind::PublisherEnded, 1);
        let end = events
            .iter()
            .find_map(publisher_end)
            .expect("the failed publisher's end is reported");
        assert_eq!(end.outcome, Outcome::Failed);
        assert_eq!(end.was_available, live);
        assert_eq!(
            end.timestamp_issue.as_ref().expect("host issue").code,
            TimestampIssueCode::VideoTimestampJump
        );
    }
    Ok(())
}

#[tokio::test]
async fn audio_repairs_report_during_preroll_and_live_and_survive_a_failing_batch()
-> Result<(), Box<dyn std::error::Error>> {
    for live in [false, true] {
        let harness = Harness::healthy();
        let start = if live { 220 } else { 10 };
        let mut packets = Vec::new();
        for index in 0..start + 22 {
            if (start..start + 20).contains(&index) {
                continue;
            }
            let pts = i64::from(index) * 1024 + if index == start + 21 { 48_000 } else { 0 };
            packets.push(audio_packet(pts));
        }
        // Put the repaired packet and the fatal successor in the same batch.
        let tail = packets.split_off(packets.len() - 2);
        let mut batches: Vec<_> = packets.into_iter().map(|packet| vec![packet]).collect();
        batches.push(tail);
        let error = harness
            .run_scripted_audio(batches)
            .await
            .expect_err("excessive gap");
        assert_eq!(
            super::timestamp_issue(&error)
                .ok_or("timestamp issue")?
                .recovery_rejection,
            Some(crate::domain::RecoveryRejection::MaximumHole)
        );
        assert_eq!(harness.store.leased(), 0);
        assert_eq!(
            harness.meters.timestamp_rejections(),
            vec![(("audio_gap".into(), "audio".into()), 1)]
        );
        let repairs = harness.meters.audio_repairs();
        assert_eq!(repairs.len(), 1);
        assert_eq!(repairs[0].1.0, 1);
        let events = harness.recorder.projected();
        assert_event_count(&events, Kind::PublisherDegraded, 1);
        let end = events
            .last()
            .and_then(publisher_end)
            .expect("the final lifecycle event ends the publisher after the excessive gap");
        assert_eq!(end.outcome, Outcome::Failed);
        assert_eq!(end.was_available, live);
        assert_eq!(end.compensation.len(), 1);
        assert!(end.compensation[0].degraded);
        assert_eq!(end.compensation[0].total_ticks, 20 * 1024);
        if live {
            assert!(harness.meters.snapshot().parts_published > 0);
            assert!(!harness.live().is_ended());
        } else {
            assert_eq!(harness.meters.snapshot().parts_published, 0);
        }
    }
    Ok(())
}

fn start_declared_cadence_normalizer(
    presentation: &PresentationPlan,
    timeline: &TimelineCalibration,
    mode: InputMode,
) -> Result<StartedNormalizer, NormalizeError> {
    let mut tracks = presentation.tracks().to_vec();
    tracks[0].video_cadence = crate::domain::VideoCadence::Fixed {
        rate: crate::domain::FrameRate::new(nz::u32!(1), nz::u32!(1)),
        source: crate::domain::CadenceSource::H264Vui,
        scope: crate::domain::CadenceScope::ProgressiveFrames,
    };
    let presentation = presentation
        .with_projected_tracks(tracks)
        .map_err(|e| NormalizeError::InvalidPlan(e.to_string().into()))?;
    start_normalizer(&presentation, timeline, mode)
}

// Keep the startup/live and strict/permissive assertions together.
#[tokio::test]
async fn input_modes_report_cadence_holes_through_sessions()
-> Result<(), Box<dyn std::error::Error>> {
    use crate::domain::TimestampIssueCode;
    for mode in [InputMode::Strict, InputMode::Permissive] {
        for live in [false, true] {
            let harness = Harness::with_normalizer(mode, start_declared_cadence_normalizer);
            let expected = if live { 3 * SECOND } else { SECOND };
            let batches =
                video_batches_before_issue(live, vec![packet(expected + SECOND / 4, true)]);
            let outcome = harness.run_scripted_video(batches).await;
            let events = harness.recorder.projected();
            let ended = events
                .iter()
                .find_map(publisher_end)
                .expect("session end reported");
            if mode == InputMode::Strict {
                let error = outcome.expect_err("strict cadence hole");
                let issue = super::timestamp_issue(&error).expect("typed timestamp failure");
                assert_eq!(issue.code, TimestampIssueCode::VideoCadenceViolation);
                assert_eq!(issue.recovery_rejection, None);
                assert!(
                    ended
                        .timestamp_issue
                        .as_ref()
                        .expect("hook issue")
                        .cadence
                        .is_some()
                );
                assert_eq!(
                    harness.meters.snapshot().parts_published,
                    if live { 2 } else { 0 }
                );
                assert!(ended.compensation.is_empty());
                assert!(harness.meters.video_compensation().is_empty());
            } else {
                outcome?;
                assert!(ended.timestamp_issue.is_none());
                assert_eq!(ended.compensation.len(), 1);
                assert_eq!(ended.compensation[0].total_ticks, 22500);
                assert_eq!(ended.compensation[0].replacement_ticks, 0);
                assert_eq!(
                    harness.meters.video_compensation(),
                    vec![(("h264".into(), "gap".into()), (1, 0.25))]
                );
            }
            assert_eq!(
                harness.meters.cadence_violations(),
                vec![("h264".into(), 1)]
            );
            assert_event_count(&events, Kind::PublisherEnded, 1);
            assert_eq!(harness.store.leased(), 0);
        }
    }
    Ok(())
}

#[tokio::test]
async fn audio_gaps_publish_during_preroll_and_live_then_report_clean_recovery()
-> Result<(), Box<dyn std::error::Error>> {
    for start in [10, 220] {
        let harness = Harness::healthy();
        let batches = (0..1800)
            .filter(|index| !(start..start + 6).contains(index))
            .map(|index| vec![audio_packet(index * 1024)])
            .collect();
        harness.run_scripted_audio(batches).await?;
        assert_eq!(harness.store.leased(), 0);
        assert!(harness.meters.timestamp_rejections().is_empty());
        assert_eq!(
            harness.meters.audio_repairs(),
            vec![(("aac".into(), "gap".into()), (1, 0.128))]
        );
        let events = harness.recorder.projected();
        assert_event_count(&events, Kind::PublisherDegraded, 1);
        assert_event_count(&events, Kind::PublisherRecovered, 1);
        let end = events
            .last()
            .and_then(publisher_end)
            .expect("the final lifecycle event ends the publisher after recovery");
        assert_eq!(end.compensation[0].total_ticks, 6144);
        assert_eq!(end.compensation[0].replacement_ticks, 0);
        assert!(!end.compensation[0].degraded);
        assert!(end.was_available);
        assert!(harness.meters.snapshot().parts_published > 0);
    }
    Ok(())
}

#[tokio::test]
async fn video_gap_notices_survive_later_batch_failure_in_preroll_and_live()
-> Result<(), Box<dyn std::error::Error>> {
    for live in [false, true] {
        let harness =
            Harness::with_normalizer(InputMode::Permissive, start_declared_cadence_normalizer);
        let expected = if live { 3 * SECOND } else { SECOND };
        let later_batch = vec![packet(expected + SECOND / 4, true), packet(expected, true)];
        let batches = video_batches_before_issue(live, later_batch);
        let error = harness
            .run_scripted_video(batches)
            .await
            .expect_err("later packet in the batch has backward DTS");
        assert_eq!(
            super::timestamp_issue(&error).ok_or("typed issue")?.code,
            crate::domain::TimestampIssueCode::VideoTimestampOrder
        );
        assert_eq!(harness.store.leased(), 0);
        assert_eq!(
            harness.meters.video_compensation(),
            vec![(("h264".into(), "gap".into()), (1, 0.25))]
        );
        let events = harness.recorder.projected();
        assert_event_count(&events, Kind::PublisherDegraded, 1);
        assert_event_count(&events, Kind::PublisherEnded, 1);
        let end = events
            .last()
            .and_then(publisher_end)
            .expect("the final lifecycle event ends the publisher after the backward DTS");
        assert_eq!(end.compensation.len(), 1);
        assert_eq!(
            end.compensation[0].method,
            crate::domain::RecoveryMethod::Gap
        );
        assert_eq!(end.compensation[0].replacement_ticks, 0);
        assert_eq!(end.compensation[0].total_ticks, 22_500);
        assert!(end.compensation[0].degraded);
        assert_eq!(end.was_available, live);
        // The later invalid packet prevents publication of the whole batch.
        // Previously published valid samples retain the existing lifecycle.
        assert_eq!(
            harness.meters.snapshot().parts_published,
            if live { 2 } else { 0 }
        );
    }
    Ok(())
}

#[tokio::test]
async fn exhausted_publisher_memory_fails_without_waiting_for_another_keyframe()
-> Result<(), Box<dyn std::error::Error>> {
    let harness = Harness::healthy();
    let mut configuration = config();
    configuration.memory_per_publisher = Some(1);
    let outcome = tokio::time::timeout(
        Duration::from_secs(1),
        harness.run_with(Ending::Eof, &configuration),
    )
    .await?;
    let error = outcome.expect_err("sample cannot fit the shared budget");
    assert!(
        error.to_string().contains("pipeline memory exhausted"),
        "{error}"
    );
    assert_eq!(harness.services.meters.snapshot().sessions_failed, 1);
    assert_eq!(harness.services.meters.snapshot().pipeline_exhaustions, 1);
    Ok(())
}

/// Every lease taken while publishing must be released once the session has
/// ended and its output has been handed to storage.
#[tokio::test]
async fn completed_session_returns_its_pipeline_budget() -> Result<(), Box<dyn std::error::Error>> {
    let harness = Harness::healthy();
    harness.run(Ending::Eof).await?;
    let budget = harness
        .budget_probe
        .lock()
        .clone()
        .ok_or("the source saw the session budget")?;
    assert!(budget.peak() > 0, "the session charged its media");
    assert_eq!(budget.used(), 0);
    assert_eq!(budget.origins(), [0; crate::domain::Origin::ALL.len()]);
    Ok(())
}
