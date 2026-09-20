//! Persistent exports, independent of the live store's retention budget.
//!
//! Media tasks only retain cheap payload handles and try a bounded queue. One
//! dedicated thread owns filesystem I/O; both unfinished segments and queued
//! files count against the same byte budget. Overload loses an entire recording
//! segment, never an apparently complete file containing only its tail.

mod filesystem;
mod pattern;
#[cfg(test)]
mod tests;

use super::hls::{HlsError, HlsPublisher, PublishOutcome, PublisherFactory};
use crate::{
    domain::{BoxFuture, Payload, StreamId},
    mux::{
        ClosedCaptionService, FinishReason, PackagedMedia, PackagedPresentation,
        PackagingRenditionId, PackagingSegmentId,
    },
    observe::{Events, NodeEvent, ProcessMeters},
};
use pattern::Pattern;
use serde::Deserialize;
use std::{
    collections::HashMap,
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
        mpsc,
    },
    time::{Duration, SystemTime},
};
use tokio::sync::Notify;
use uuid::Uuid;

/// A local archive. Patterns are compiled and paths checked at startup.
#[derive(Clone, Debug, Eq, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub dir: PathBuf,
    #[serde(default = "default_pattern")]
    pub pattern: String,
    #[serde(default = "default_queue")]
    pub queue_capacity: usize,
    /// Includes open segments, queued jobs, and the active filesystem write.
    #[serde(default = "default_bytes")]
    pub maximum_pending_bytes: usize,
}
fn default_pattern() -> String {
    "{stream}/{publication}/{time:%Y/%m/%d}/{rendition}_{segment}.mp4".into()
}
fn default_queue() -> usize {
    128
}
fn default_bytes() -> usize {
    256 * 1024 * 1024
}

impl Config {
    pub fn validate(&self) -> Result<(), String> {
        if self.dir.as_os_str().is_empty() || self.dir.to_string_lossy().contains("://") {
            return Err("record.dir must be a filesystem path, not a URL".into());
        }
        if self.queue_capacity == 0 || self.maximum_pending_bytes == 0 {
            return Err("record queue_capacity and maximum_pending_bytes must be nonzero".into());
        }
        Pattern::parse(&self.pattern).map(|_| ())
    }
}

struct Shared {
    maximum: usize,
    bytes: AtomicUsize,
    jobs: AtomicUsize,
    finished: Notify,
    events: Events,
    meters: ProcessMeters,
    /// Segments lost since the current failure window opened; zero means
    /// healthy. A single counter carries both the edge and the total, so a
    /// loss concurrent with a recovery cannot be counted in the closed window
    /// while reopening a new one with an empty count.
    lost: AtomicUsize,
}
impl Shared {
    /// Reports one segment's worth of loss.
    ///
    /// The counter climbs on every call; the event only on the edge that opens
    /// a failure window. A disk that stays broken therefore produces one
    /// `RecordingFailed` and a rising counter rather than a stream of identical
    /// events with no aggregate.
    fn failure(&self, stream: &StreamId, reason: impl Into<String>) {
        self.meters.recording_segment_lost();
        if self.lost.fetch_add(1, Ordering::AcqRel) != 0 {
            return;
        }
        self.events.emit(NodeEvent::RecordingFailed {
            stream: stream.clone(),
            reason: reason.into(),
        });
    }
    /// Closes the failure window after a segment reaches the archive.
    ///
    /// Called only by the worker, because a successful commit is the one fact
    /// that says writes resumed; producer-side successes say the segment was
    /// accepted, not that it landed.
    fn success(&self) {
        let lost = self.lost.swap(0, Ordering::AcqRel);
        if lost != 0 {
            self.events.emit(NodeEvent::RecordingRecovered { lost });
        }
    }
    fn reserve(self: &Arc<Self>, bytes: usize) -> Option<Reservation> {
        self.bytes
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |used| {
                used.checked_add(bytes)
                    .filter(|total| *total <= self.maximum)
            })
            .ok()?;
        Some(Reservation {
            shared: self.clone(),
            bytes,
        })
    }
}
struct Reservation {
    shared: Arc<Shared>,
    bytes: usize,
}
impl Drop for Reservation {
    fn drop(&mut self) {
        self.shared.bytes.fetch_sub(self.bytes, Ordering::AcqRel);
    }
}
struct Job {
    stream: StreamId,
    path: PathBuf,
    payloads: Vec<Payload>,
    _reservations: Vec<Reservation>,
}

/// Cloneable producer; the worker exits when the last producer is dropped.
#[derive(Clone, derive_more::Debug)]
#[debug("Recorder")]
pub struct Recorder {
    #[debug(skip)]
    jobs: mpsc::SyncSender<Job>,
    #[debug(skip)]
    shared: Arc<Shared>,
    pattern: Pattern,
}
impl Recorder {
    pub fn start(config: &Config, events: Events, meters: ProcessMeters) -> std::io::Result<Self> {
        config.validate().map_err(std::io::Error::other)?;
        let root = filesystem::root(&config.dir)?;
        let pattern = Pattern::parse(&config.pattern).map_err(std::io::Error::other)?;
        let shared = Arc::new(Shared {
            maximum: config.maximum_pending_bytes,
            bytes: AtomicUsize::new(0),
            jobs: AtomicUsize::new(0),
            finished: Notify::new(),
            events,
            meters,
            lost: AtomicUsize::new(0),
        });
        let (jobs, receiver) = mpsc::sync_channel::<Job>(config.queue_capacity);
        let worker = shared.clone();
        std::thread::Builder::new()
            .name("rushls-record".into())
            .spawn(move || {
                for job in receiver {
                    match filesystem::write(&root, &job.path, &job.payloads) {
                        Ok(()) => worker.success(),
                        Err(error) => {
                            worker.failure(&job.stream, format!("{}: {error}", job.path.display()));
                        }
                    }
                    drop(job);
                    worker.jobs.fetch_sub(1, Ordering::AcqRel);
                    worker.finished.notify_one();
                }
            })?;
        Ok(Self {
            jobs,
            shared,
            pattern,
        })
    }

    /// Called after publishers stop. Accepted writes get a bounded shutdown drain.
    pub async fn drain(&self, timeout: Duration) {
        let drained = tokio::time::timeout(timeout, async {
            loop {
                let notified = self.shared.finished.notified();
                if self.shared.jobs.load(Ordering::Acquire) == 0 {
                    break;
                }
                notified.await;
            }
        })
        .await;
        if drained.is_err() {
            self.shared.events.emit(NodeEvent::RecordingDrainExpired {
                pending: self.shared.jobs.load(Ordering::Acquire),
            });
        }
    }

    fn submit(&self, job: Job) {
        self.shared.jobs.fetch_add(1, Ordering::AcqRel);
        if let Err(error) = self.jobs.try_send(job) {
            self.shared.jobs.fetch_sub(1, Ordering::AcqRel);
            let job = match error {
                mpsc::TrySendError::Full(job) | mpsc::TrySendError::Disconnected(job) => job,
            };
            self.shared.failure(
                &job.stream,
                "recording queue unavailable; segment was not recorded",
            );
        }
    }
}

/// Adds recording after successful live publication; revoked writes never export.
pub struct RecordingFactory {
    pub inner: Arc<dyn PublisherFactory>,
    pub recorder: Recorder,
}
impl PublisherFactory for RecordingFactory {
    fn start(
        &self,
        stream: &StreamId,
        presentation: Arc<PackagedPresentation>,
    ) -> Result<Box<dyn HlsPublisher>, HlsError> {
        // Validate names before accepting any media, without filesystem work on
        // the media path. A fresh UUID prevents restart/reconnect collisions.
        let publication = Uuid::now_v7().to_string();
        let inner = self.inner.start(stream, presentation.clone())?;
        let mut renditions = HashMap::new();
        for rendition in presentation.renditions.iter() {
            if let Err(error) = self.recorder.pattern.expand(
                stream.0.as_ref(),
                &publication,
                &rendition.key.0,
                0,
                SystemTime::now(),
                false,
            ) {
                self.recorder.shared.failure(stream, error);
                return Ok(inner);
            }
            renditions.insert(
                rendition.packaging_rendition_id,
                Track {
                    name: rendition.key.0.to_string(),
                    text: super::uri::is_text(rendition.config.segment_format),
                    initialization: None,
                    open: None,
                },
            );
        }
        Ok(Box::new(RecordingPublisher {
            inner,
            recorder: self.recorder.clone(),
            stream: stream.clone(),
            publication,
            renditions,
        }))
    }
}
struct Track {
    name: String,
    text: bool,
    initialization: Option<Payload>,
    open: Option<Open>,
}
struct Open {
    id: PackagingSegmentId,
    // None means the whole segment was dropped, including all following parts.
    body: Option<(Vec<Payload>, Vec<Reservation>)>,
}
struct RecordingPublisher {
    inner: Box<dyn HlsPublisher>,
    recorder: Recorder,
    stream: StreamId,
    publication: String,
    renditions: HashMap<PackagingRenditionId, Track>,
}
impl RecordingPublisher {
    fn record(&mut self, media: PackagedMedia) {
        let Some(track) = self.renditions.get_mut(&media.rendition_id()) else {
            return;
        };
        let (id, payload, complete) = match media {
            PackagedMedia::Initialization(init) => {
                track.initialization = Some(init.payload);
                return;
            }
            PackagedMedia::Chunk(chunk) => (chunk.packaging_segment_id, Some(chunk.payload), false),
            PackagedMedia::Segment(segment) => {
                (segment.packaging_segment_id, Some(segment.payload), true)
            }
            PackagedMedia::Gap(_) => return,
            PackagedMedia::SegmentCompleted(segment) => (segment.packaging_segment_id, None, true),
        };
        if track.open.as_ref().is_none_or(|open| open.id != id) {
            let body = track.initialization.as_ref().and_then(|init| {
                self.recorder
                    .shared
                    .reserve(init.len())
                    .map(|reservation| (vec![init.clone()], vec![reservation]))
            });
            if body.is_none() {
                self.recorder.shared.failure(
                    &self.stream,
                    "missing initialization or recording byte budget exhausted",
                );
            }
            track.open = Some(Open { id, body });
        }
        let open = track.open.as_mut().expect("opened above");
        if let Some(payload) = payload
            && let Some((payloads, reservations)) = &mut open.body
        {
            // Even zero-byte subtitle parts consume handles; cap that overhead.
            if let Some(reservation) = self
                .recorder
                .shared
                .reserve(payload.len().max(128))
                .filter(|_| payloads.len() < 16_384)
            {
                // All packagers emit initialization separately, including
                // WebVTT. Never sniff cue text for a header: a valid cue ID
                // can itself begin with "WEBVTT".
                payloads.push(payload);
                reservations.push(reservation);
            } else {
                open.body = None;
                self.recorder.shared.failure(
                    &self.stream,
                    "recording byte or part budget exhausted; segment was not recorded",
                );
            }
        }
        if complete {
            let open = track.open.take().expect("opened above");
            if let Some((payloads, reservations)) = open.body {
                match self.recorder.pattern.expand(
                    self.stream.0.as_ref(),
                    &self.publication,
                    &track.name,
                    id.0,
                    SystemTime::now(),
                    track.text,
                ) {
                    Ok(path) => self.recorder.submit(Job {
                        stream: self.stream.clone(),
                        path,
                        payloads,
                        _reservations: reservations,
                    }),
                    Err(error) => self.recorder.shared.failure(&self.stream, error),
                }
            }
        }
    }
}
impl HlsPublisher for RecordingPublisher {
    fn publisher_disconnected(&mut self) {
        self.inner.publisher_disconnected();
    }
    fn is_backpressured(&self) -> bool {
        self.inner.is_backpressured()
    }
    fn ready(&mut self) -> BoxFuture<'_, Result<(), HlsError>> {
        self.inner.ready()
    }
    fn write(&mut self, media: PackagedMedia) -> Result<PublishOutcome, HlsError> {
        let outcome = self.inner.write(media.clone())?;
        if outcome == PublishOutcome::Published {
            self.record(media);
        }
        Ok(outcome)
    }
    fn declare_closed_captions(&mut self, services: Arc<[ClosedCaptionService]>) -> bool {
        self.inner.declare_closed_captions(services)
    }
    fn finish(&mut self, reason: FinishReason) -> Result<(), HlsError> {
        self.inner.finish(reason)
    }
}
