//! Test composition root: temp dirs, Services with [record] on, burst run.
//!
//! Mirrors server/runtime.rs service assembly but keeps the Recorder handle so
//! the test can drain and assert zero loss. The session cadence follows the
//! e2e config, which defaults to the shipped 6s/1s cadence and only shortens
//! it when RUSHLS_TEST_RECORD_E2E_SEGMENT_SECS says so.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use parking_lot::Mutex;
use rushls::admission::{OpenStreamAuthenticator, StreamPolicy};
use rushls::delivery::hls::{StorePublisherFactory, StreamStore};
use rushls::delivery::record::{Config as RecordConfig, Recorder, RecordingFactory};
use rushls::media::PassThroughNormalizerFactory;
use rushls::mux::PassThroughMuxerFactory;
use rushls::domain::SessionId;
use rushls::observe::{EventObserver, Events, NodeEvent, ProcessMeters, SessionEvent};
use rushls::session::{PendingPermit, Registry, Services, SessionConfig, SessionError, SessionOutcome, run_session};

use super::config::E2eConfig;
use super::publish::{BurstMpegTs, live_camera};

/// Scratch root: <tmp>/rushls-record-e2e-<pid>-<nanos>/ with src.ts, archive/.
pub struct WorkDir {
    root: PathBuf,
    archive: PathBuf,
    keep: bool,
    owned: bool,
}

impl WorkDir {
    pub fn new(_cfg: &E2eConfig) -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
        let nanos = SystemTime::now().duration_since(SystemTime::UNIX_EPOCH)?.as_nanos();
        let root = std::env::temp_dir().join(format!("rushls-record-e2e-{}-{nanos}", std::process::id()));
        let archive = root.join("archive");
        std::fs::create_dir_all(&archive)?;
        let keep = std::env::var("RUSHLS_TEST_RECORD_E2E_KEEP").is_ok();
        Ok(Self { root, archive, keep, owned: true })
    }

    pub fn path(&self) -> &Path {
        &self.root
    }
    pub fn archive(&self) -> &Path {
        &self.archive
    }
    pub fn keep(&self) -> bool {
        self.keep
    }
    /// Forget the Drop cleanup so failures stay inspectable with KEEP=1.
    pub fn leak(&mut self) {
        self.owned = false;
    }
}

impl Drop for WorkDir {
    fn drop(&mut self) {
        if self.owned && !self.keep {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }
}

#[derive(Default)]
struct Log(Mutex<Vec<NodeEvent>>);

impl EventObserver for Log {
    fn observe(&self, _s: SessionId, _e: SessionEvent) {}
    fn observe_node(&self, event: NodeEvent) {
        self.0.lock().push(event);
    }
}

#[allow(dead_code)]
pub struct RecordOutcome {
    pub session: SessionOutcome,
    pub node_events: Vec<NodeEvent>,
    pub recording_lost: u64,
    pub files: Vec<PathBuf>,
}

fn record_config(archive: &Path, pattern: &str) -> RecordConfig {
    RecordConfig {
        dir: archive.to_path_buf(),
        pattern: pattern.into(),
        queue_capacity: 128,
        maximum_pending_bytes: 256 * 1024 * 1024,
    }
}

/// One assembled stack (Services + Recorder) that can run several sequential
/// burst sessions into the same archive. A reconnect is exactly that: the
/// encoder drops, dials again, and the server mints a fresh publication id
/// per accepted session, so segment numbers restart while the archive keeps
/// both halves apart via the publication placeholder in the pattern.
pub struct TestRig {
    archive: PathBuf,
    services: Services,
    recorder: Recorder,
    log: Arc<Log>,
    meters: ProcessMeters,
    session_cfg: SessionConfig,
}

/// Files plus health signals left after the recorder drained.
pub struct DrainedArchive {
    pub node_events: Vec<NodeEvent>,
    pub recording_lost: u64,
    pub files: Vec<PathBuf>,
}

impl TestRig {
    /// Build Services exactly like Node::new does, but keep the Recorder
    /// handle so the test can drain and assert zero loss afterwards. The
    /// session cadence comes from the e2e config: prod 6s/1s unless the
    /// segment-target override shortens it for rapid churn.
    pub fn start(archive: &Path, pattern: &str, cfg: &E2eConfig) -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
        let log = Arc::new(Log::default());
        let events = Events::new(log.clone());
        let meters = ProcessMeters::default();
        let store = StreamStore::new(rushls::delivery::hls::StoreLimits::default());

        let recorder = Recorder::start(&record_config(archive, pattern), events.clone(), meters.clone())?;
        let inner = StorePublisherFactory::new(store);
        let publishers = RecordingFactory { inner: Arc::new(inner), recorder: recorder.clone() };

        let services = Services {
            authenticator: Arc::new(OpenStreamAuthenticator::new(StreamPolicy::permissive())),
            normalizers: Arc::new(PassThroughNormalizerFactory),
            muxers: Arc::new(PassThroughMuxerFactory),
            publishers: Arc::new(publishers),
            sessions: Registry::with_capacity(16),
            meters: meters.clone(),
            events,
        };
        // Permissive input throughout; the burst comes from the input being
        // fully buffered, not from a special ceiling. Only the segment
        // cadence follows the e2e config.
        let session_cfg = SessionConfig { segmentation: cfg.segmentation_policy(), ..SessionConfig::default() };
        eprintln!("record e2e: session cadence {}s segments", cfg.segment_secs);
        Ok(Self { archive: archive.to_path_buf(), services, recorder, log, meters, session_cfg })
    }

    /// Publish one fully-buffered input as fast as the session demuxes.
    pub async fn run_burst(&self, bytes: Vec<u8>) -> Result<SessionOutcome, SessionError> {
        eprintln!("record e2e: publishing {:.1} MiB with no pacing", bytes.len() as f64 / 1_048_576.0);
        let pending: Box<dyn rushls::source::PendingPublish> = Box::new(BurstMpegTs {
            request: live_camera(),
            bytes,
        });
        let started = std::time::Instant::now();
        let session = run_session(pending, &self.services, &self.session_cfg, PendingPermit::unlimited()).await?;
        eprintln!("record e2e: session {session:?} after {:?}", started.elapsed());
        Ok(session)
    }

    /// Drain the recorder and hand back files + health signals.
    pub async fn drain_and_collect(self) -> Result<DrainedArchive, Box<dyn std::error::Error + Send + Sync>> {
        self.recorder.drain(Duration::from_secs(30)).await;
        let files = collect_files(&self.archive)?;
        eprintln!("record e2e: archive holds {} files", files.len());
        let node_events = std::mem::take(&mut *self.log.0.lock());
        let recording_lost = self.meters.snapshot().recording_segments_lost;
        Ok(DrainedArchive { node_events, recording_lost, files })
    }
}

/// Single-session convenience over TestRig: publish the whole file in one
/// burst, require a clean end, drain, and hand back files + health signals.
pub async fn publish_and_record(
    cfg: &E2eConfig,
    work: &WorkDir,
    ts_path: &Path,
) -> Result<RecordOutcome, Box<dyn std::error::Error + Send + Sync>> {
    let bytes = std::fs::read(ts_path)?;
    let rig = TestRig::start(work.archive(), "{rendition}_{segment}.mp4", cfg)?;
    let session = rig.run_burst(bytes).await?;
    if session != SessionOutcome::Ended {
        return Err(format!("burst session did not end cleanly: {session:?}").into());
    }
    let drained = rig.drain_and_collect().await?;
    let _ = cfg;
    Ok(RecordOutcome { session, node_events: drained.node_events, recording_lost: drained.recording_lost, files: drained.files })
}

fn collect_files(archive: &Path) -> std::io::Result<Vec<PathBuf>> {
    fn visit(dir: &Path, out: &mut Vec<PathBuf>) -> std::io::Result<()> {
        for item in std::fs::read_dir(dir)? {
            let path = item?.path();
            if path.is_dir() {
                visit(&path, out)?;
            } else if !path.file_name().map(|n| n.to_string_lossy().starts_with(".rushls-")).unwrap_or(false) {
                out.push(path);
            }
        }
        Ok(())
    }
    let mut files = Vec::new();
    visit(archive, &mut files)?;
    files.sort();
    Ok(files)
}
