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
    fn source_progress(&self, bytes: u64, packets: u64, lost: u64);

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

/// Process-wide totals and session lifecycle tallies.
#[derive(Clone, Debug, Default)]
pub struct ProcessMeters {
    counters: Arc<ProcessCounters>,
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
        bytes_received: u64 = Counter(
            "rushls_bytes_received_total",
            "Bytes received from publishers."
        ),
        packets_received: u64 = Counter(
            "rushls_packets_received_total",
            "Packets received from publishers."
        ),
        packets_lost: u64 = Counter(
            "rushls_packets_lost_total",
            "Publisher packets reported lost."
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
    pub fn session_started(&self) {
        add(&self.counters.sessions_started, 1);
    }

    pub fn session_completed(&self) {
        add(&self.counters.sessions_completed, 1);
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
    /// Uses the runtime clock so liveness can be exercised deterministically
    /// rather than by sleeping in tests.
    started_at: Instant,
    process: ProcessMeters,
    bytes_received: AtomicU64,
    packets_received: AtomicU64,
    packets_lost: AtomicU64,
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
    source_seen: LivenessMark,
    media_seen: LivenessMark,
    publication_seen: LivenessMark,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct MeterSnapshot {
    pub bytes_received: u64,
    pub packets_received: u64,
    pub packets_lost: u64,
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
}

// Declared apart from the storage above because the storage is not a plain
// bank of atomics: durations are held as nanoseconds and the peaks accumulate
// with `fetch_max`. What is exported is uniform even though what is kept is
// not.
series! {
    MeterSnapshot {
        Counter("rushls_session_bytes_received_total",
            "Bytes received by an active session.")
            = |snapshot: &MeterSnapshot| snapshot.bytes_received,
        Counter("rushls_session_packets_received_total",
            "Packets received by an active session.")
            = |snapshot: &MeterSnapshot| snapshot.packets_received,
        Counter("rushls_session_packets_lost_total",
            "Packets reported lost by an active session.")
            = |snapshot: &MeterSnapshot| snapshot.packets_lost,
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
        Gauge("rushls_session_publisher_backpressured",
            "Whether an active publisher is currently backpressured.")
            = |snapshot: &MeterSnapshot| snapshot.publisher_backpressured,
    }
}

impl SessionMeters {
    pub fn new(process: ProcessMeters) -> Self {
        Self {
            counters: Arc::new(SessionCounters {
                started_at: Instant::now(),
                process,
                bytes_received: AtomicU64::new(0),
                packets_received: AtomicU64::new(0),
                packets_lost: AtomicU64::new(0),
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
            packets_lost: get(&counters.packets_lost),
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
        }
    }

    pub fn started_at(&self) -> Instant {
        self.counters.started_at
    }

    /// How long ago the source last delivered data, or `None` if it never has.
    pub fn source_idle_for(&self, now: Instant) -> Option<Duration> {
        self.counters
            .source_seen
            .idle_for(self.counters.started_at, now)
    }

    /// How long ago normalization last produced a sample.
    pub fn media_idle_for(&self, now: Instant) -> Option<Duration> {
        self.counters
            .media_seen
            .idle_for(self.counters.started_at, now)
    }

    /// How long ago a chunk or complete segment became available to viewers.
    pub fn publication_idle_for(&self, now: Instant) -> Option<Duration> {
        self.counters
            .publication_seen
            .idle_for(self.counters.started_at, now)
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
#[derive(Debug, Default)]
struct LivenessMark(AtomicU64);

impl LivenessMark {
    /// Records "now", measured from the session's own start.
    ///
    /// The stored value is biased by one so that zero — the initial state —
    /// unambiguously means never, without needing a second flag to say so.
    fn mark(&self, started_at: Instant) {
        let elapsed = Instant::now().saturating_duration_since(started_at);
        let nanos =
            u64::try_from(elapsed.as_nanos().min(u128::from(u64::MAX - 1))).unwrap_or(u64::MAX);
        self.0.store(nanos + 1, Ordering::Relaxed);
    }

    /// How long ago this was last marked, or `None` if it never was.
    fn idle_for(&self, started_at: Instant, now: Instant) -> Option<Duration> {
        let stored = self.0.load(Ordering::Relaxed);
        if stored == 0 {
            return None;
        }

        let seen_at = started_at + Duration::from_nanos(stored - 1);
        Some(now.saturating_duration_since(seen_at))
    }
}

impl SourceMeters for SessionCounters {
    fn source_progress(&self, bytes: u64, packets: u64, lost: u64) {
        add(&self.bytes_received, bytes);
        add(&self.packets_received, packets);
        add(&self.packets_lost, lost);
        add(&self.process.counters.bytes_received, bytes);
        add(&self.process.counters.packets_received, packets);
        add(&self.process.counters.packets_lost, lost);
        if bytes > 0 || packets > 0 {
            self.source_seen.mark(self.started_at);
        }
    }

    fn codec_parameters_changed(&self) {
        self.process.codec_parameters_changed();
    }
}

impl MediaMeters for SessionCounters {
    fn media_progress(&self, packets: u64, samples: u64) {
        add(&self.packets_normalized, packets);
        add(&self.samples_normalized, samples);
        raise(&self.peak_packets_per_batch, packets);
        raise(&self.peak_samples_per_batch, samples);
        if samples > 0 {
            self.media_seen.mark(self.started_at);
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
            self.publication_seen.mark(self.started_at);
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

        meters.source_view().source_progress(1_024, 8, 1);
        meters.media_view().media_progress(8, 8);
        meters.delivery_view().delivery_progress(2, 1);

        let session = meters.snapshot();
        assert_eq!(session.bytes_received, 1_024);
        assert_eq!(session.packets_lost, 1);
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

        meters.source_view().source_progress(1, 1, 0);
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
        meters.source_view().source_progress(0, 0, 3);
        tokio::time::advance(Duration::from_secs(1)).await;

        assert_eq!(meters.source_idle_for(Instant::now()), None);
        assert_eq!(meters.snapshot().packets_lost, 3);
    }
}
