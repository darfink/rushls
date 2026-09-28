use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::Duration,
};

use tokio::time::Instant;

use super::counters::{counters, series};

/// Volume produced by the ingest front end.
///
/// Signatures are batch-shaped on purpose: a source counts into local variables
/// while it drains a socket read and calls this once, so the atomic traffic is
/// per batch rather than per packet.
pub trait SourceMeters: Send + Sync {
    fn pipeline_budget(&self) -> Option<&crate::domain::PipelineBudget> {
        None
    }

    fn source_progress(&self, bytes: u64, packets: u64);

    /// Reported when the input changes codec parameters mid-stream.
    ///
    /// Recorded by the source that detected it, immediately before it fails the
    /// session. The alternative — having the session recognise the cause by
    /// matching into a nested error after the fact — couples the runner to the
    /// exact shape of every error path that can carry the news.
    fn codec_parameters_changed(&self);
}

/// Volume produced by normalization.
pub trait MediaMeters: Send + Sync {
    fn pipeline_budget(&self) -> Option<&crate::domain::PipelineBudget> {
        None
    }

    fn video_interval(&self, _observation: crate::domain::VideoTimestampObservation) {}
    fn compensation(&self, _notice: &crate::domain::NormalizationNotice) {}
    fn track_input(&self, _id: crate::domain::TrackId, _bytes: usize) {}
    fn track_normalized(&self, _id: crate::domain::TrackId, _pts: i64, _duration: u64) {}

    fn media_progress(&self, packets: u64, samples: u64);

    /// Records the current normalized-media lead, accumulated pacing delay,
    /// and whether the publisher is presently being slowed by backpressure.
    fn pacing_observation(
        &self,
        media_lead: Duration,
        pacing_delay: Duration,
        publisher_backpressured: bool,
    );
}

/// Volume produced by container muxing.
pub trait MuxMeters: Send + Sync {
    fn mux_progress(&self, chunks: u64, segments: u64);
}

/// Volume made available to viewers.
pub trait DeliveryMeters: Send + Sync {
    fn delivery_progress(&self, parts: u64, segments: u64);
}

type AudioRepairCounters = std::collections::BTreeMap<(String, String), (u64, f64)>;

/// Process-wide totals and session lifecycle tallies.
#[derive(Clone, Debug, Default)]
pub struct ProcessMeters {
    video_intervals:
        Arc<parking_lot::Mutex<std::collections::BTreeMap<String, super::DurationHistogram>>>,
    video_compensation: Arc<parking_lot::Mutex<AudioRepairCounters>>,
    cadence_violations: Arc<parking_lot::Mutex<std::collections::BTreeMap<String, u64>>>,
    audio_repairs: Arc<parking_lot::Mutex<AudioRepairCounters>>,
    counters: Arc<ProcessCounters>,
    // Keys come exclusively from closed enums, never publisher-supplied labels.
    timestamp_rejections:
        Arc<parking_lot::Mutex<std::collections::BTreeMap<(String, String), u64>>>,
}

counters! {
    ProcessCounters => ProcessSnapshot {
        sessions_started: u64 = Counter(
            "rushls_sessions_started_total",
            "Publishing sessions started."
        ),
        sessions_completed: u64 = Counter(
            "rushls_sessions_completed_total",
            "Publishing sessions completed successfully."
        ),
        part_contract_failures: u64 = Counter("rushls_part_contract_failures_total", "Publications rejected by the part contract."),
        boundary_contract_failures: u64 = Counter("rushls_boundary_contract_failures_total", "Publications rejected by segment boundary constraints."),
        coordinator_limit_failures: u64 = Counter("rushls_coordinator_limit_failures_total", "Publications exceeding coordinator resource limits."),
        pipeline_exhaustions: u64 = Counter(
            "rushls_pipeline_exhaustions_total",
            "Failed memory reservations reported by completed or failed publishing sessions."
        ),
        sessions_failed: u64 = Counter(
            "rushls_sessions_failed_total",
            "Publishing sessions that failed."
        ),
        sessions_replaced: u64 = Counter(
            "rushls_sessions_replaced_total",
            "Publishing sessions displaced by a takeover."
        ),
        publishers_rejected: u64 = Counter(
            "rushls_publishers_rejected_total",
            "Publishers rejected before a session started."
        ),
        codec_parameter_changes: u64 = Counter(
            "rushls_codec_parameter_changes_total",
            "Mid-stream codec parameter changes detected."
        ),
        unhealthy_terminations: u64 = Counter(
            "rushls_unhealthy_terminations_total",
            "Sessions stopped by health supervision."
        ),
        drain_failures: u64 = Counter(
            "rushls_drain_failures_total",
            "Sessions that failed while flushing their tail."
        ),
        /// The rate a stalled recorder runs at. Carried here rather than in an
        /// event because [`NodeEvent::RecordingFailed`] is reported on the edge
        /// of a failure window, not once per segment; without this the count of
        /// lost segments would be readable only from the log stream.
        ///
        /// [`NodeEvent::RecordingFailed`]: super::NodeEvent::RecordingFailed
        recording_segments_lost: u64 = Counter(
            "rushls_recording_segments_lost_total",
            "Archive segments not written because recording failed."
        ),
        bytes_received: u64 = Counter(
            "rushls_source_payload_bytes_total",
            "Demultiplexed publisher payload bytes, excluding transport overhead."
        ),
        packets_received: u64 = Counter(
            "rushls_source_packets_total",
            "Demultiplexed media packets from publishers, not transport packets."
        ),
        parts_published: u64 = Counter(
            "rushls_parts_published_total",
            "HLS parts made available to viewers."
        ),
        segments_published: u64 = Counter(
            "rushls_segments_published_total",
            "HLS segments made available to viewers."
        ),
        tls_handshakes_completed: u64 = Counter(
            "rushls_tls_handshakes_completed_total",
            "TLS handshakes completed."
        ),
        /// Includes handshakes that simply timed out, which is what a port scan
        /// looks like. A counter rather than an event precisely because the rate
        /// is chosen by whoever is connecting.
        tls_handshakes_failed: u64 = Counter(
            "rushls_tls_handshakes_failed_total",
            "TLS handshakes that failed or timed out."
        ),
    }
}

impl ProcessMeters {
    pub fn video_intervals(&self) -> Vec<(String, super::DurationHistogram)> {
        self.video_intervals
            .lock()
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect()
    }
    pub fn video_compensation(&self) -> Vec<((String, String), (u64, f64))> {
        self.video_compensation
            .lock()
            .iter()
            .map(|(k, v)| (k.clone(), *v))
            .collect()
    }
    pub fn cadence_violations(&self) -> Vec<(String, u64)> {
        self.cadence_violations
            .lock()
            .iter()
            .map(|(k, v)| (k.clone(), *v))
            .collect()
    }

    pub fn audio_repairs(&self) -> Vec<((String, String), (u64, f64))> {
        self.audio_repairs
            .lock()
            .iter()
            .map(|(key, value)| (key.clone(), *value))
            .collect()
    }

    pub fn timestamp_rejected(&self, issue: &crate::domain::TimestampIssue) {
        if issue.code == crate::domain::TimestampIssueCode::VideoCadenceViolation {
            *self
                .cadence_violations
                .lock()
                .entry(format!("{:?}", issue.codec).to_lowercase())
                .or_default() += 1;
        }
        let mut counts = self.timestamp_rejections.lock();
        let value = counts
            .entry((
                issue.code.to_string(),
                format!("{:?}", issue.media_kind).to_lowercase(),
            ))
            .or_default();
        *value = value.saturating_add(1);
    }

    pub fn timestamp_rejections(&self) -> Vec<((String, String), u64)> {
        self.timestamp_rejections
            .lock()
            .iter()
            .map(|(key, value)| (key.clone(), *value))
            .collect()
    }

    pub fn session_started(&self) {
        add(&self.counters.sessions_started, 1);
    }

    pub fn session_completed(&self) {
        add(&self.counters.sessions_completed, 1);
    }

    pub fn segmentation_failed(&self, error: &crate::mux::MuxError) {
        match error {
            crate::mux::MuxError::Part { .. } => add(&self.counters.part_contract_failures, 1),
            crate::mux::MuxError::Boundary { .. } | crate::mux::MuxError::BoundaryWindow { .. } => {
                add(&self.counters.boundary_contract_failures, 1);
            }
            crate::mux::MuxError::CoordinatorLimit { .. } => {
                add(&self.counters.coordinator_limit_failures, 1);
            }
            _ => {}
        }
    }

    pub fn pipeline_exhaustions(&self, count: u64) {
        add(&self.counters.pipeline_exhaustions, count);
    }

    pub fn session_failed(&self) {
        add(&self.counters.sessions_failed, 1);
    }

    pub fn session_replaced(&self) {
        add(&self.counters.sessions_replaced, 1);
    }

    pub fn publisher_rejected(&self) {
        add(&self.counters.publishers_rejected, 1);
    }

    /// Recorded where the change is detected, not reconstructed afterwards by
    /// matching into a nested session error.
    pub fn codec_parameters_changed(&self) {
        add(&self.counters.codec_parameter_changes, 1);
    }

    pub fn unhealthy_termination(&self) {
        add(&self.counters.unhealthy_terminations, 1);
    }

    /// A session that ran but could not flush its tail.
    ///
    /// Kept apart from [`Self::session_failed`] deliberately. The session
    /// delivered media and reached a legitimate outcome; only its last few
    /// frames are in doubt. Folding the two together would either hide a
    /// recurring muxer fault inside the ordinary failure count or make every
    /// mid-segment disconnect look like a node-level fault.
    pub fn drain_failed(&self) {
        add(&self.counters.drain_failures, 1);
    }

    /// Records one archive segment that was not written.
    ///
    /// Called for every loss, so it climbs throughout a failure window while
    /// the matching event fires only at the edges.
    pub fn recording_segment_lost(&self) {
        add(&self.counters.recording_segments_lost, 1);
    }

    pub fn tls_handshake_completed(&self) {
        add(&self.counters.tls_handshakes_completed, 1);
    }

    pub fn tls_handshake_failed(&self) {
        add(&self.counters.tls_handshakes_failed, 1);
    }

    pub fn snapshot(&self) -> ProcessSnapshot {
        self.counters.snapshot()
    }
}

/// Per-session volume plus the liveness marks health evaluation reads.
///
/// One handle is created per session and rolls its updates into the process
/// totals in the same call, so no layer has to report the same number twice.
#[derive(Clone, Debug)]
pub struct SessionMeters {
    counters: Arc<SessionCounters>,
}

#[derive(Debug)]
struct SessionCounters {
    budget: crate::domain::PipelineBudget,
    /// Uses the runtime clock so liveness can be exercised deterministically
    /// rather than by sleeping in tests.
    started_at: Instant,
    process: ProcessMeters,
    bytes_received: AtomicU64,
    packets_received: AtomicU64,
    packets_normalized: AtomicU64,
    samples_normalized: AtomicU64,
    chunks_muxed: AtomicU64,
    segments_muxed: AtomicU64,
    parts_published: AtomicU64,
    segments_published: AtomicU64,
    /// High-water marks, kept so a session that spiked can be told apart from
    /// one that ran evenly. Buffer capacity follows the largest batch a session
    /// ever saw, so this is the number that explains its resident memory.
    peak_packets_per_batch: AtomicU64,
    peak_samples_per_batch: AtomicU64,
    media_lead_nanos: AtomicU64,
    pacing_delay_nanos: AtomicU64,
    publisher_backpressured: AtomicBool,
    tracks: super::tracks::TrackMeters,
    source_seen: LivenessMark,
    media_seen: LivenessMark,
    publication_seen: LivenessMark,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct MeterSnapshot {
    pub bytes_received: u64,
    pub packets_received: u64,
    pub packets_normalized: u64,
    pub samples_normalized: u64,
    pub chunks_muxed: u64,
    pub segments_muxed: u64,
    pub parts_published: u64,
    pub segments_published: u64,
    pub peak_packets_per_batch: u64,
    pub peak_samples_per_batch: u64,
    pub media_lead: Duration,
    pub pacing_delay: Duration,
    pub publisher_backpressured: bool,
    /// Configured accounted-allocation limit, excluding uninstrumented native
    /// buffers and process overhead. This is not an RSS ceiling.
    pub pipeline_bytes: u64,
    pub pipeline_used_bytes: u64,
    pub pipeline_peak_bytes: u64,
    pub pipeline_failures: u64,
    pub pipeline_working_bytes: u64,
    pub pipeline_origins: [u64; 5],
}

// Declared apart from the storage above because the storage is not a plain
// bank of atomics: durations are held as nanoseconds and the peaks accumulate
// with `fetch_max`. What is exported is uniform even though what is kept is
// not.
series! {
    MeterSnapshot {
        Counter("rushls_session_source_payload_bytes_total",
            "Demultiplexed payload bytes received by an active session.")
            = |snapshot: &MeterSnapshot| snapshot.bytes_received,
        Counter("rushls_session_source_packets_total",
            "Demultiplexed media packets received by an active session.")
            = |snapshot: &MeterSnapshot| snapshot.packets_received,
        Gauge("rushls_session_transport_loss_observable",
            "Whether transport packet loss is measured. Current sources do not provide this measurement.")
            = |_snapshot: &MeterSnapshot| false,
        Counter("rushls_session_packets_normalized_total",
            "Packets normalized by an active session.")
            = |snapshot: &MeterSnapshot| snapshot.packets_normalized,
        Counter("rushls_session_samples_normalized_total",
            "Samples emitted by normalization for an active session.")
            = |snapshot: &MeterSnapshot| snapshot.samples_normalized,
        Counter("rushls_session_chunks_muxed_total",
            "Media chunks muxed by an active session.")
            = |snapshot: &MeterSnapshot| snapshot.chunks_muxed,
        Counter("rushls_session_segments_muxed_total",
            "Segments muxed by an active session.")
            = |snapshot: &MeterSnapshot| snapshot.segments_muxed,
        Counter("rushls_session_parts_published_total",
            "HLS parts published by an active session.")
            = |snapshot: &MeterSnapshot| snapshot.parts_published,
        Counter("rushls_session_segments_published_total",
            "HLS segments published by an active session.")
            = |snapshot: &MeterSnapshot| snapshot.segments_published,
        Gauge("rushls_session_peak_packets_per_batch",
            "Largest packet batch observed by an active session.")
            = |snapshot: &MeterSnapshot| snapshot.peak_packets_per_batch,
        Gauge("rushls_session_peak_samples_per_batch",
            "Largest sample batch observed by an active session.")
            = |snapshot: &MeterSnapshot| snapshot.peak_samples_per_batch,
        Gauge("rushls_session_media_lead_seconds",
            "Current normalized-media lead for an active session.")
            = |snapshot: &MeterSnapshot| snapshot.media_lead.as_secs_f64(),
        Counter("rushls_session_pacing_delay_seconds_total",
            "Pacing delay accumulated by an active session.")
            = |snapshot: &MeterSnapshot| snapshot.pacing_delay.as_secs_f64(),
        Gauge("rushls_session_pacing_active",
            "Whether the media pacer is currently withholding input; excludes store waits.")
            = |snapshot: &MeterSnapshot| snapshot.publisher_backpressured,
        Gauge("rushls_session_pipeline_capacity_bytes",
            "Bytes one publisher may hold before the store, which \
             `memory_per_stream` does not cover.")
            = |snapshot: &MeterSnapshot| snapshot.pipeline_bytes,
        Gauge("rushls_session_pipeline_used_bytes", "Current accounted pipeline bytes.")
            = |snapshot: &MeterSnapshot| snapshot.pipeline_used_bytes,
        Gauge("rushls_session_pipeline_peak_bytes", "Peak accounted pipeline bytes.")
            = |snapshot: &MeterSnapshot| snapshot.pipeline_peak_bytes,
        Gauge("rushls_session_pipeline_working_bytes", "Current temporary serialization reservations within the pipeline total.")
            = |snapshot: &MeterSnapshot| snapshot.pipeline_working_bytes,
        Counter("rushls_session_pipeline_exhaustions_total", "Failed pipeline memory reservations.")
            = |snapshot: &MeterSnapshot| snapshot.pipeline_failures,
    }
}

impl SessionMeters {
    pub fn new(process: ProcessMeters) -> Self {
        Self::with_budget(
            process,
            crate::domain::PipelineBudget::new(crate::domain::PipelineBudget::DEFAULT_LIMIT),
        )
    }

    pub fn with_budget(process: ProcessMeters, budget: crate::domain::PipelineBudget) -> Self {
        Self {
            counters: Arc::new(SessionCounters {
                budget,
                started_at: Instant::now(),
                process,
                bytes_received: AtomicU64::new(0),
                packets_received: AtomicU64::new(0),
                packets_normalized: AtomicU64::new(0),
                samples_normalized: AtomicU64::new(0),
                chunks_muxed: AtomicU64::new(0),
                segments_muxed: AtomicU64::new(0),
                parts_published: AtomicU64::new(0),
                segments_published: AtomicU64::new(0),
                peak_packets_per_batch: AtomicU64::new(0),
                peak_samples_per_batch: AtomicU64::new(0),
                media_lead_nanos: AtomicU64::new(0),
                pacing_delay_nanos: AtomicU64::new(0),
                publisher_backpressured: AtomicBool::new(false),
                tracks: super::tracks::TrackMeters::default(),
                source_seen: LivenessMark::default(),
                media_seen: LivenessMark::default(),
                publication_seen: LivenessMark::default(),
            }),
        }
    }

    /// Hands a stage exactly the reporting surface it needs.
    ///
    /// Every view is the same allocation; narrowing is about what a stage can
    /// see, not about creating separate sinks.
    pub fn tracks(&self) -> &super::tracks::TrackMeters {
        &self.counters.tracks
    }

    pub fn source_view(&self) -> Arc<dyn SourceMeters> {
        Arc::clone(&self.counters) as Arc<dyn SourceMeters>
    }

    pub fn media_view(&self) -> Arc<dyn MediaMeters> {
        Arc::clone(&self.counters) as Arc<dyn MediaMeters>
    }

    pub fn mux_view(&self) -> Arc<dyn MuxMeters> {
        Arc::clone(&self.counters) as Arc<dyn MuxMeters>
    }

    pub fn delivery_view(&self) -> Arc<dyn DeliveryMeters> {
        Arc::clone(&self.counters) as Arc<dyn DeliveryMeters>
    }

    /// The process totals this session rolls into.
    ///
    /// Available so lifecycle outcomes are recorded where they are decided
    /// rather than reconstructed later by inspecting an error.
    pub fn process(&self) -> &ProcessMeters {
        &self.counters.process
    }

    pub fn snapshot(&self) -> MeterSnapshot {
        let counters = &self.counters;
        MeterSnapshot {
            bytes_received: get(&counters.bytes_received),
            packets_received: get(&counters.packets_received),
            packets_normalized: get(&counters.packets_normalized),
            samples_normalized: get(&counters.samples_normalized),
            chunks_muxed: get(&counters.chunks_muxed),
            segments_muxed: get(&counters.segments_muxed),
            parts_published: get(&counters.parts_published),
            segments_published: get(&counters.segments_published),
            peak_packets_per_batch: get(&counters.peak_packets_per_batch),
            peak_samples_per_batch: get(&counters.peak_samples_per_batch),
            media_lead: Duration::from_nanos(get(&counters.media_lead_nanos)),
            pacing_delay: Duration::from_nanos(get(&counters.pacing_delay_nanos)),
            publisher_backpressured: counters.publisher_backpressured.load(Ordering::Relaxed),
            pipeline_bytes: counters.budget.limit() as u64,
            pipeline_used_bytes: counters.budget.used() as u64,
            pipeline_peak_bytes: counters.budget.peak() as u64,
            pipeline_failures: counters.budget.failures() as u64,
            pipeline_working_bytes: counters.budget.working() as u64,
            pipeline_origins: counters.budget.origins(),
        }
    }

    pub fn started_at(&self) -> Instant {
        self.counters.started_at
    }

    /// How long ago the source last delivered data, or `None` if it never has.
    pub fn source_idle_for(&self, now: Instant) -> Option<Duration> {
        self.counters
            .source_seen
            .idle_for(self.counters.active_elapsed(now))
    }

    /// How long ago normalization last produced a sample.
    pub fn media_idle_for(&self, now: Instant) -> Option<Duration> {
        self.counters
            .media_seen
            .idle_for(self.counters.active_elapsed(now))
    }

    /// How long ago a chunk or complete segment became available to viewers.
    pub fn publication_idle_for(&self, now: Instant) -> Option<Duration> {
        self.counters
            .publication_seen
            .idle_for(self.counters.active_elapsed(now))
    }

    pub fn publisher_backpressured(&self) -> bool {
        self.counters
            .publisher_backpressured
            .load(Ordering::Relaxed)
    }
}

/// When a stage last showed a sign of life, or never.
///
/// Health evaluation asks "how long since X last happened?", and the answer has
/// to distinguish *never* from *just now* — a source that has produced nothing
/// since the session began is as stalled as one that stopped, but only if the
/// two are told apart, and only if "never" does not read as "at time zero".
///
/// Stored as one atomic so a stage can mark it from the same batch update it
/// was already making, without a lock and without a second timestamp source to
/// drift from the counters an operator reads.
///
/// Measured on the session's *active* clock rather than wall clock — see
/// [`SessionCounters::active_elapsed`].
#[derive(Debug, Default)]
struct LivenessMark(AtomicU64);

impl LivenessMark {
    /// Records the current active-clock reading.
    ///
    /// The stored value is biased by one so that zero — the initial state —
    /// unambiguously means never, without needing a second flag to say so.
    fn mark(&self, active_elapsed: Duration) {
        let nanos = u64::try_from(active_elapsed.as_nanos().min(u128::from(u64::MAX - 1)))
            .unwrap_or(u64::MAX);
        self.0.store(nanos + 1, Ordering::Relaxed);
    }

    /// How long ago this was last marked, or `None` if it never was.
    fn idle_for(&self, active_elapsed: Duration) -> Option<Duration> {
        let stored = self.0.load(Ordering::Relaxed);
        if stored == 0 {
            return None;
        }

        Some(active_elapsed.saturating_sub(Duration::from_nanos(stored - 1)))
    }
}

impl SessionCounters {
    /// Elapsed session time with deliberate pacing sleeps removed.
    ///
    /// Liveness is measured on this clock rather than on wall clock, because a
    /// publisher the pacer is sleeping is idle by this node's own instruction.
    /// Charging that against a stall deadline would drop exactly the publishers
    /// a `ceiling` is throttling correctly.
    ///
    /// Discounting the sleep rather than suppressing the check is what keeps
    /// the alarm armed throughout: a stage that genuinely stops producing while
    /// the pacer happens to be sleeping still ages on this clock, because only
    /// the sleeping is subtracted and not the silence around it.
    fn active_elapsed(&self, now: Instant) -> Duration {
        let paced = Duration::from_nanos(self.pacing_delay_nanos.load(Ordering::Relaxed));
        now.saturating_duration_since(self.started_at)
            .saturating_sub(paced)
    }
}

impl SourceMeters for SessionCounters {
    fn pipeline_budget(&self) -> Option<&crate::domain::PipelineBudget> {
        Some(&self.budget)
    }

    fn source_progress(&self, bytes: u64, packets: u64) {
        add(&self.bytes_received, bytes);
        add(&self.packets_received, packets);
        add(&self.process.counters.bytes_received, bytes);
        add(&self.process.counters.packets_received, packets);
        if bytes > 0 || packets > 0 {
            self.source_seen.mark(self.active_elapsed(Instant::now()));
        }
    }

    fn codec_parameters_changed(&self) {
        self.process.codec_parameters_changed();
    }
}

impl MediaMeters for SessionCounters {
    fn pipeline_budget(&self) -> Option<&crate::domain::PipelineBudget> {
        Some(&self.budget)
    }

    fn video_interval(&self, observation: crate::domain::VideoTimestampObservation) {
        self.process
            .video_intervals
            .lock()
            .entry(format!("{:?}", observation.codec).to_lowercase())
            .or_default()
            .observe(observation.timebase.ticks_to_duration(observation.ticks));
    }

    fn compensation(&self, notice: &crate::domain::NormalizationNotice) {
        if matches!(
            notice.transition,
            crate::domain::RecoveryTransition::Recovered
                | crate::domain::RecoveryTransition::Unavailable
        ) {
            return;
        }
        let status = &notice.status;
        let mut counts = if status.media_kind == crate::domain::MediaKind::Video {
            *self
                .process
                .cadence_violations
                .lock()
                .entry(format!("{:?}", status.codec).to_lowercase())
                .or_default() += 1;
            self.process.video_compensation.lock()
        } else {
            self.process.audio_repairs.lock()
        };
        let count = counts
            .entry((
                format!("{:?}", status.codec).to_lowercase(),
                status.method.to_string(),
            ))
            .or_default();
        count.0 = count.0.saturating_add(1);
        let duration = status.timebase.ticks_to_duration(status.missing_ticks);
        count.1 += duration.as_secs_f64();
    }

    fn track_input(&self, id: crate::domain::TrackId, bytes: usize) {
        self.tracks.input(id, bytes);
    }
    fn track_normalized(&self, id: crate::domain::TrackId, pts: i64, duration: u64) {
        self.tracks.normalized(id, pts, duration);
    }

    fn media_progress(&self, packets: u64, samples: u64) {
        add(&self.packets_normalized, packets);
        add(&self.samples_normalized, samples);
        raise(&self.peak_packets_per_batch, packets);
        raise(&self.peak_samples_per_batch, samples);
        if samples > 0 {
            self.media_seen.mark(self.active_elapsed(Instant::now()));
        }
    }

    fn pacing_observation(
        &self,
        media_lead: Duration,
        pacing_delay: Duration,
        publisher_backpressured: bool,
    ) {
        self.media_lead_nanos.store(
            u64::try_from(media_lead.as_nanos().min(u128::from(u64::MAX))).unwrap_or(u64::MAX),
            Ordering::Relaxed,
        );
        add(
            &self.pacing_delay_nanos,
            u64::try_from(pacing_delay.as_nanos().min(u128::from(u64::MAX))).unwrap_or(u64::MAX),
        );
        self.publisher_backpressured
            .store(publisher_backpressured, Ordering::Relaxed);
    }
}

impl MuxMeters for SessionCounters {
    fn mux_progress(&self, chunks: u64, segments: u64) {
        add(&self.chunks_muxed, chunks);
        add(&self.segments_muxed, segments);
    }
}

impl DeliveryMeters for SessionCounters {
    fn delivery_progress(&self, parts: u64, segments: u64) {
        add(&self.parts_published, parts);
        add(&self.segments_published, segments);
        add(&self.process.counters.parts_published, parts);
        add(&self.process.counters.segments_published, segments);
        if parts > 0 || segments > 0 {
            self.publication_seen
                .mark(self.active_elapsed(Instant::now()));
        }
    }
}

#[inline]
fn add(counter: &AtomicU64, value: u64) {
    if value > 0 {
        counter.fetch_add(value, Ordering::Relaxed);
    }
}

/// Records a new high-water mark, leaving the counter alone if it already
/// holds a larger one.
#[inline]
fn raise(counter: &AtomicU64, value: u64) {
    if value > 0 {
        counter.fetch_max(value, Ordering::Relaxed);
    }
}

#[inline]
fn get(counter: &AtomicU64) -> u64 {
    counter.load(Ordering::Relaxed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_volume_rolls_into_process_totals() {
        let process = ProcessMeters::default();
        let meters = SessionMeters::new(process.clone());

        meters.source_view().source_progress(1_024, 8);
        meters.media_view().media_progress(8, 8);
        meters.delivery_view().delivery_progress(2, 1);

        let session = meters.snapshot();
        assert_eq!(session.bytes_received, 1_024);
        assert_eq!(session.samples_normalized, 8);
        assert_eq!(session.parts_published, 2);

        let totals = process.snapshot();
        assert_eq!(totals.bytes_received, 1_024);
        assert_eq!(totals.packets_received, 8);
        assert_eq!(totals.parts_published, 2);
        assert_eq!(totals.segments_published, 1);
    }

    #[tokio::test(start_paused = true)]
    async fn liveness_marks_start_unset_and_track_the_last_update() {
        let meters = SessionMeters::new(ProcessMeters::default());
        tokio::time::advance(Duration::from_secs(4)).await;

        assert_eq!(meters.source_idle_for(Instant::now()), None);
        assert_eq!(meters.publication_idle_for(Instant::now()), None);

        meters.source_view().source_progress(1, 1);
        tokio::time::advance(Duration::from_secs(1)).await;

        assert_eq!(
            meters.source_idle_for(Instant::now()),
            Some(Duration::from_secs(1))
        );
        assert_eq!(meters.media_idle_for(Instant::now()), None);
    }

    #[test]
    fn peak_batch_sizes_record_the_largest_batch_not_the_latest() {
        let meters = SessionMeters::new(ProcessMeters::default());
        let media = meters.media_view();

        media.media_progress(10, 40);
        media.media_progress(400, 1_600);
        media.media_progress(12, 48);

        let snapshot = meters.snapshot();
        assert_eq!(snapshot.peak_packets_per_batch, 400);
        assert_eq!(snapshot.peak_samples_per_batch, 1_600);
        assert_eq!(snapshot.packets_normalized, 422);
    }

    #[tokio::test(start_paused = true)]
    async fn zero_volume_updates_do_not_mark_liveness() {
        let meters = SessionMeters::new(ProcessMeters::default());
        meters.source_view().source_progress(0, 0);
        tokio::time::advance(Duration::from_secs(1)).await;

        assert_eq!(meters.source_idle_for(Instant::now()), None);
    }
}
