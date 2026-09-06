//! Overflow tier: completed parents the memory cap cannot hold.
//!
//! New media still lands in RAM. This module only writes what the store has
//! already decided to spill, and only reads what a viewer asked for by URI.
//! Playlist projection never comes here. A restart does not rehydrate the
//! catalog: generation directories are process-lifetime. The directory lock
//! refuses a second process, so cutover is stop-then-start. Reap on open
//! removes crash leftovers that still have `owner.pid`; a generation whose
//! owner pid is still alive is left alone so a reused pid cannot delete files
//! still in use. Directories without `owner.pid` are not generations and are
//! not removed.

use std::{
    collections::HashMap,
    fs::{self, File, OpenOptions},
    io::{self, Write},
    os::{fd::AsRawFd, unix::fs::OpenOptionsExt},
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicU64, AtomicUsize, Ordering},
        mpsc::{self, Receiver, SyncSender, TrySendError},
    },
    thread::{self, JoinHandle},
};

use parking_lot::Mutex;
use tokio::sync::Semaphore;

use crate::domain::{Payload, RenditionId, StreamId};

use super::{PartId, SegmentId};

/// How many spill jobs may sit unwritten before the store treats disk as full.
const SPILL_QUEUE_BOUND: usize = 32;
/// Concurrent DVR reads, in bytes, so seek traffic cannot allocate unbounded RAM.
#[cfg(not(test))]
const MAX_IN_FLIGHT_READ_BYTES: u32 = 32 * 1024 * 1024;
/// Tight enough that a test can fill it with two small files.
#[cfg(test)]
const MAX_IN_FLIGHT_READ_BYTES: u32 = 32;

/// Operator-facing disk overflow for one process.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DiskLimits {
    pub directory: PathBuf,
    pub maximum_payload_bytes: usize,
}

/// A payload that has been written and is addressable by path and length.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DiskRef {
    pub path: Arc<PathBuf>,
    pub len: usize,
}

/// Bytes that currently live in RAM, or a locator for a spilled copy.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum HeldBytes {
    Memory(Payload),
    Disk(DiskRef),
}

impl HeldBytes {
    pub fn len(&self) -> usize {
        match self {
            Self::Memory(payload) => payload.len(),
            Self::Disk(disk) => disk.len,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub const fn is_memory(&self) -> bool {
        matches!(self, Self::Memory(_))
    }

    pub fn memory_bytes(&self) -> usize {
        match self {
            Self::Memory(payload) => payload.len(),
            Self::Disk(_) => 0,
        }
    }

    pub fn disk_bytes(&self) -> usize {
        match self {
            Self::Memory(_) => 0,
            Self::Disk(disk) => disk.len,
        }
    }

    pub fn as_memory(&self) -> Option<&Payload> {
        match self {
            Self::Memory(payload) => Some(payload),
            Self::Disk(_) => None,
        }
    }

    pub fn as_bytes(&self) -> &[u8] {
        match self {
            Self::Memory(payload) => payload.as_bytes(),
            Self::Disk(_) => &[],
        }
    }

    pub fn bytes(&self) -> Option<&bytes::Bytes> {
        match self {
            Self::Memory(payload) => Some(payload.bytes()),
            Self::Disk(_) => None,
        }
    }

    pub fn unlink_disk(&self) {
        if let Self::Disk(disk) = self {
            let _ = fs::remove_file(&*disk.path);
        }
    }
}

impl From<Payload> for HeldBytes {
    fn from(payload: Payload) -> Self {
        Self::Memory(payload)
    }
}

/// Process-wide spill worker and generation directory.
pub struct DiskTier {
    shared: Arc<DiskShared>,
    jobs: Option<SyncSender<SpillJob>>,
    worker: Option<JoinHandle<()>>,
}

/// State the worker may hold without keeping the job channel open.
struct DiskShared {
    generation: PathBuf,
    maximum_payload_bytes: usize,
    pending: AtomicUsize,
    spills_failed: AtomicU64,
    /// In-flight and retired epochs. A successor may reuse the public stream
    /// id, so retirement deletes one epoch and waits for its jobs first.
    epochs: Mutex<HashMap<(StreamId, u64), EpochJobs>>,
    read_bytes: Arc<Semaphore>,
    /// Exclusive lock on `dir`. Dropped with the generation so a peer cannot
    /// reap files while a spill is still finishing.
    _lock: File,
}

#[derive(Default)]
struct EpochJobs {
    pending: usize,
    reap: bool,
}

impl std::fmt::Debug for DiskTier {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DiskTier")
            .field("generation", &self.shared.generation)
            .field("maximum_payload_bytes", &self.shared.maximum_payload_bytes)
            .finish_non_exhaustive()
    }
}

pub struct SpillJob {
    pub live: std::sync::Weak<super::stream::LiveStream>,
    pub stream: StreamId,
    /// Distinguishes two `LiveStream` values that share a public stream id.
    pub epoch: u64,
    pub rendition: RenditionId,
    pub segment: SegmentId,
    pub objects: Vec<SpillObject>,
}

pub struct SpillObject {
    pub kind: SpillKind,
    pub payload: Payload,
    pub gzip: Option<Payload>,
}

#[derive(Clone, Copy, Debug)]
pub enum SpillKind {
    Segment,
    Part(PartId),
}

pub struct SpillOutcome {
    pub rendition: RenditionId,
    pub segment: SegmentId,
    pub objects: Vec<SpilledObject>,
}

pub struct SpilledObject {
    pub kind: SpillKind,
    pub payload: DiskRef,
    pub gzip: Option<DiskRef>,
}

#[derive(Debug, thiserror::Error)]
pub enum DiskError {
    #[error("could not create disk retention directory {path}: {source}")]
    Create { path: PathBuf, source: io::Error },
    #[error("could not write {path}: {source}")]
    Write { path: PathBuf, source: io::Error },
    #[error("could not read {path}: {source}")]
    Read { path: PathBuf, source: io::Error },
    #[error("stream name {0} is not a safe disk path")]
    UnsafeStream(StreamId),
    #[error("disk retention directory {path} is in use by another rushls process")]
    InUse { path: PathBuf },
}

impl DiskTier {
    pub fn open(limits: &DiskLimits) -> Result<Arc<Self>, DiskError> {
        fs::create_dir_all(&limits.directory).map_err(|source| DiskError::Create {
            path: limits.directory.clone(),
            source,
        })?;
        let lock = lock_directory(&limits.directory)?;
        reap_dead_generations(&limits.directory);
        let generation = unique_generation(&limits.directory);
        fs::create_dir_all(&generation).map_err(|source| DiskError::Create {
            path: generation.clone(),
            source,
        })?;
        write_owner_pid(&generation)?;

        let (jobs, rx) = mpsc::sync_channel(SPILL_QUEUE_BOUND);
        let shared = Arc::new(DiskShared {
            generation: generation.clone(),
            maximum_payload_bytes: limits.maximum_payload_bytes,
            pending: AtomicUsize::new(0),
            spills_failed: AtomicU64::new(0),
            epochs: Mutex::new(HashMap::new()),
            read_bytes: Arc::new(Semaphore::new(MAX_IN_FLIGHT_READ_BYTES as usize)),
            _lock: lock,
        });
        let worker_shared = Arc::clone(&shared);
        let worker = thread::Builder::new()
            .name("rushls-disk-spill".into())
            .spawn(move || spill_loop(&worker_shared, &rx))
            .map_err(|source| DiskError::Create {
                path: generation,
                source,
            })?;
        Ok(Arc::new(Self {
            shared,
            jobs: Some(jobs),
            worker: Some(worker),
        }))
    }

    pub fn maximum_payload_bytes(&self) -> usize {
        self.shared.maximum_payload_bytes
    }

    pub fn queue_is_full(&self) -> bool {
        self.spill_pending() >= SPILL_QUEUE_BOUND
    }

    pub fn spill_pending(&self) -> usize {
        self.shared.pending.load(Ordering::Relaxed)
    }

    pub fn spills_failed(&self) -> u64 {
        self.shared.spills_failed.load(Ordering::Relaxed)
    }

    pub fn try_enqueue(&self, job: SpillJob) -> bool {
        let Some(jobs) = self.jobs.as_ref() else {
            return false;
        };
        self.shared.note_enqueued(&job.stream, job.epoch);
        self.shared.pending.fetch_add(1, Ordering::Relaxed);
        match jobs.try_send(job) {
            Ok(()) => true,
            Err(TrySendError::Full(job) | TrySendError::Disconnected(job)) => {
                self.shared.pending.fetch_sub(1, Ordering::Relaxed);
                self.shared.note_finished(&job.stream, job.epoch);
                drop(job);
                false
            }
        }
    }

    /// Reads a spilled payload into RAM.
    ///
    /// The file is opened after the byte semaphore is acquired. Capacity
    /// eviction unlinks immediately, so a queued reader can 404 if the victim
    /// was dropped while it waited. An already-open fd stays readable on POSIX;
    /// this path is the queue, not that fd.
    pub async fn read(&self, disk: &DiskRef) -> Result<Payload, DiskError> {
        let want = u32::try_from(disk.len)
            .unwrap_or(u32::MAX)
            .clamp(1, MAX_IN_FLIGHT_READ_BYTES);
        let permit = self
            .shared
            .read_bytes
            .clone()
            .acquire_many_owned(want)
            .await
            .map_err(|_| DiskError::Read {
                path: (*disk.path).clone(),
                source: io::Error::other("disk read semaphore closed"),
            })?;
        let path = Arc::clone(&disk.path);
        let result = tokio::task::spawn_blocking(move || fs::read(&*path))
            .await
            .map_err(|source| DiskError::Read {
                path: (*disk.path).clone(),
                source: io::Error::other(source),
            })?
            .map_err(|source| DiskError::Read {
                path: (*disk.path).clone(),
                source,
            })?;
        drop(permit);
        Ok(Payload::from(result))
    }

    /// Drops one LiveStream's files, after any in-flight spill for that epoch.
    ///
    /// The public stream id may already belong to a successor, so this never
    /// deletes sibling epoch directories.
    pub fn remove_stream(&self, stream: &StreamId, epoch: u64) {
        self.shared.request_reap(stream, epoch);
    }
}

impl Drop for DiskTier {
    fn drop(&mut self) {
        // Closing the sender unblocks the worker; joining it drops the last
        // shared handle so the generation directory and lock are released.
        drop(self.jobs.take());
        if let Some(worker) = self.worker.take() {
            // A job that upgraded its stream may drop the last DiskTier Arc on
            // this thread. Joining here would wait for ourselves.
            if worker.thread().id() != thread::current().id() {
                let _ = worker.join();
            }
        }
    }
}

impl Drop for DiskShared {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.generation);
    }
}

fn spill_loop(shared: &DiskShared, rx: &Receiver<SpillJob>) {
    while let Ok(job) = rx.recv() {
        // Upgrade before any write: a retired stream's public id may already
        // belong to a new LiveStream, and those paths must not be reused.
        if let Some(live) = job.live.upgrade() {
            if shared.is_reaping(&job.stream, job.epoch) {
                live.abort_spill(job.rendition, job.segment);
            } else {
                match write_job(shared, &job) {
                    Ok(outcome) => live.finish_spill(&outcome),
                    Err(error) => {
                        shared.spills_failed.fetch_add(1, Ordering::Relaxed);
                        tracing::warn!(
                            stream = %job.stream,
                            error = %error,
                            "disk spill failed; media stays in memory"
                        );
                        live.abort_spill(job.rendition, job.segment);
                    }
                }
            }
        }
        shared.pending.fetch_sub(1, Ordering::Relaxed);
        shared.note_finished(&job.stream, job.epoch);
    }
}

fn write_job(shared: &DiskShared, job: &SpillJob) -> Result<SpillOutcome, DiskError> {
    let mut objects = Vec::with_capacity(job.objects.len());
    for object in &job.objects {
        match spill_object(shared, job, object) {
            Ok(spilled) => objects.push(spilled),
            Err(error) => {
                forget_spilled(&objects);
                return Err(error);
            }
        }
    }
    Ok(SpillOutcome {
        rendition: job.rendition,
        segment: job.segment,
        objects,
    })
}

fn spill_object(
    shared: &DiskShared,
    job: &SpillJob,
    object: &SpillObject,
) -> Result<SpilledObject, DiskError> {
    let name = match object.kind {
        SpillKind::Segment => format!("segment-{}.bin", job.segment.0),
        SpillKind::Part(id) => format!("part-{}.bin", id.0),
    };
    let path = shared.object_path(&job.stream, job.epoch, job.rendition, &name)?;
    write_atomic(&path, object.payload.as_bytes())?;
    let payload = DiskRef {
        len: object.payload.len(),
        path: Arc::new(path.clone()),
    };
    let gzip = if let Some(gzip) = &object.gzip {
        let gzip_path = path.with_extension("bin.gz");
        if let Err(error) = write_atomic(&gzip_path, gzip.as_bytes()) {
            let _ = fs::remove_file(&path);
            return Err(error);
        }
        Some(DiskRef {
            len: gzip.len(),
            path: Arc::new(gzip_path),
        })
    } else {
        None
    };
    Ok(SpilledObject {
        kind: object.kind,
        payload,
        gzip,
    })
}

impl DiskShared {
    fn lock_epochs(&self) -> parking_lot::MutexGuard<'_, HashMap<(StreamId, u64), EpochJobs>> {
        self.epochs.lock()
    }

    fn note_enqueued(&self, stream: &StreamId, epoch: u64) {
        self.lock_epochs()
            .entry((stream.clone(), epoch))
            .or_default()
            .pending += 1;
    }

    fn is_reaping(&self, stream: &StreamId, epoch: u64) -> bool {
        self.lock_epochs()
            .get(&(stream.clone(), epoch))
            .is_some_and(|jobs| jobs.reap)
    }

    fn request_reap(&self, stream: &StreamId, epoch: u64) {
        let key = (stream.clone(), epoch);
        let mut epochs = self.lock_epochs();
        let entry = epochs.entry(key.clone()).or_default();
        entry.reap = true;
        if entry.pending != 0 {
            return;
        }
        epochs.remove(&key);
        drop(epochs);
        self.remove_epoch_dir(stream, epoch);
    }

    fn note_finished(&self, stream: &StreamId, epoch: u64) {
        let key = (stream.clone(), epoch);
        let mut epochs = self.lock_epochs();
        let Some(entry) = epochs.get_mut(&key) else {
            return;
        };
        entry.pending = entry.pending.saturating_sub(1);
        let reap = entry.reap && entry.pending == 0;
        if entry.pending == 0 {
            epochs.remove(&key);
        }
        drop(epochs);
        if reap {
            self.remove_epoch_dir(stream, epoch);
        }
    }

    fn remove_epoch_dir(&self, stream: &StreamId, epoch: u64) {
        let Ok(relative) = stream_relative(stream) else {
            return;
        };
        let stream_dir = self.generation.join(relative);
        let _ = fs::remove_dir_all(stream_dir.join(format!("epoch-{epoch}")));
        // Leave sibling epochs (a republished stream with the same public id).
        let _ = fs::remove_dir(&stream_dir);
    }

    fn object_path(
        &self,
        stream: &StreamId,
        epoch: u64,
        rendition: RenditionId,
        name: &str,
    ) -> Result<PathBuf, DiskError> {
        let mut path = self.generation.join(stream_relative(stream)?);
        path.push(format!("epoch-{epoch}"));
        path.push(format!("rendition-{}", rendition.0));
        fs::create_dir_all(&path).map_err(|source| DiskError::Create {
            path: path.clone(),
            source,
        })?;
        path.push(name);
        Ok(path)
    }
}

fn write_atomic(path: &Path, bytes: &[u8]) -> Result<(), DiskError> {
    let tmp = path.with_extension("tmp");
    let result = write_atomic_inner(path, &tmp, bytes);
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
        let _ = fs::remove_file(path);
    }
    result
}

fn write_atomic_inner(path: &Path, tmp: &Path, bytes: &[u8]) -> Result<(), DiskError> {
    let mut file = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o644)
        .open(tmp)
        .map_err(|source| DiskError::Write {
            path: tmp.to_path_buf(),
            source,
        })?;
    file.write_all(bytes).map_err(|source| DiskError::Write {
        path: tmp.to_path_buf(),
        source,
    })?;
    file.sync_all().map_err(|source| DiskError::Write {
        path: tmp.to_path_buf(),
        source,
    })?;
    fs::rename(tmp, path).map_err(|source| DiskError::Write {
        path: path.to_path_buf(),
        source,
    })?;
    if let Some(parent) = path.parent() {
        let dir = File::open(parent).map_err(|source| DiskError::Write {
            path: parent.to_path_buf(),
            source,
        })?;
        dir.sync_all().map_err(|source| DiskError::Write {
            path: parent.to_path_buf(),
            source,
        })?;
    }
    Ok(())
}

pub(crate) fn forget_spilled(objects: &[SpilledObject]) {
    for object in objects {
        let _ = fs::remove_file(&*object.payload.path);
        if let Some(gzip) = &object.gzip {
            let _ = fs::remove_file(&*gzip.path);
        }
    }
}

fn lock_directory(root: &Path) -> Result<File, DiskError> {
    let path = root.join("rushls.lock");
    let file = OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .open(&path)
        .map_err(|source| DiskError::Create {
            path: path.clone(),
            source,
        })?;
    // Safety: `flock` on a file we exclusively opened; LOCK_NB fails rather
    // than blocking a second node on the same directory.
    let result = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if result != 0 {
        return Err(DiskError::InUse { path });
    }
    Ok(file)
}

fn unique_generation(root: &Path) -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |duration| duration.as_nanos());
    root.join(format!("{}-{nanos}", std::process::id()))
}

fn write_owner_pid(generation: &Path) -> Result<(), DiskError> {
    let path = generation.join("owner.pid");
    fs::write(&path, format!("{}\n", std::process::id()))
        .map_err(|source| DiskError::Write { path, source })
}

fn reap_dead_generations(root: &Path) {
    let Ok(entries) = fs::read_dir(root) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        // Only a directory this process created (one that has owner.pid) is a
        // generation. Anything else under dir is left alone.
        if !path.join("owner.pid").is_file() {
            continue;
        }
        if generation_is_live(&path) {
            continue;
        }
        let _ = fs::remove_dir_all(path);
    }
}

fn generation_is_live(generation: &Path) -> bool {
    let Ok(contents) = fs::read_to_string(generation.join("owner.pid")) else {
        return false;
    };
    let Ok(pid) = contents.trim().parse::<i32>() else {
        return false;
    };
    pid_is_live(pid)
}

fn pid_is_live(pid: i32) -> bool {
    // Safety: `kill(pid, 0)` probes existence and never delivers a signal.
    // ESRCH means the process is gone; EPERM means it exists but we cannot
    // signal it, which still makes its generation live.
    let result = unsafe { libc::kill(pid, 0) };
    result == 0 || std::io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH)
}

fn stream_relative(stream: &StreamId) -> Result<PathBuf, DiskError> {
    let mut path = PathBuf::from("streams");
    let mut any = false;
    for segment in stream.as_str().split('/') {
        if segment.is_empty() || segment == "." || segment == ".." {
            return Err(DiskError::UnsafeStream(stream.clone()));
        }
        if Path::new(segment).is_absolute() {
            return Err(DiskError::UnsafeStream(stream.clone()));
        }
        path.push(segment);
        any = true;
    }
    if any {
        Ok(path)
    } else {
        Err(DiskError::UnsafeStream(stream.clone()))
    }
}

#[cfg(test)]
mod tests {
    use std::{
        sync::{Arc, Weak},
        time::{Duration, Instant},
    };

    use crate::domain::{Payload, RenditionId, StreamId};

    use super::{
        super::{LiveStream, RetentionPolicy, SegmentId},
        *,
    };

    fn scratch(name: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "rushls-disk-{name}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_nanos())
        ));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).expect("scratch dir");
        path
    }

    fn fifo(root: &Path, name: &str) -> PathBuf {
        use std::os::unix::ffi::OsStrExt;
        let path = root.join(name);
        let cstr = std::ffi::CString::new(path.as_os_str().as_bytes()).expect("fifo path");
        // Safety: `path` is a unique temp path we created; `mkfifo` only
        // creates a named pipe at that path.
        assert_eq!(unsafe { libc::mkfifo(cstr.as_ptr(), 0o600) }, 0);
        path
    }

    fn wait_idle(tier: &DiskTier) {
        let start = Instant::now();
        while tier.spill_pending() != 0 {
            assert!(
                start.elapsed() < Duration::from_secs(2),
                "timed out waiting for the spill worker"
            );
            thread::sleep(Duration::from_millis(2));
        }
    }

    fn epoch_dir(tier: &DiskTier, stream: &StreamId, epoch: u64) -> PathBuf {
        tier.shared
            .generation
            .join(stream_relative(stream).expect("test stream names are safe"))
            .join(format!("epoch-{epoch}"))
    }

    #[test]
    fn reap_removes_a_dead_generation_and_leaves_a_live_one() {
        let root = scratch("reap");
        let dead = root.join("dead-gen");
        fs::create_dir_all(&dead).unwrap();
        fs::write(dead.join("owner.pid"), "999999999\n").unwrap();
        fs::write(dead.join("orphan.bin"), b"x").unwrap();

        let peer = root.join("peer-gen");
        fs::create_dir_all(&peer).unwrap();
        fs::write(peer.join("owner.pid"), format!("{}\n", std::process::id())).unwrap();

        let unrelated = root.join("not-a-generation");
        fs::create_dir_all(&unrelated).unwrap();
        fs::write(unrelated.join("keep.bin"), b"x").unwrap();

        let live = DiskTier::open(&DiskLimits {
            directory: root.clone(),
            maximum_payload_bytes: 1024,
        })
        .expect("open");

        assert!(
            !dead.exists(),
            "pid 999999999 is not running; the leftover generation is reaped"
        );
        assert!(
            peer.exists(),
            "a generation whose owner pid is still live is left alone"
        );
        assert!(
            unrelated.exists(),
            "a directory without owner.pid is not a generation and is left alone"
        );
        assert!(live.shared.generation.exists());
        assert!(
            live.shared.generation.join("owner.pid").exists(),
            "the live generation keeps its lock file"
        );
        drop(live);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn a_second_process_cannot_share_the_directory() {
        let root = scratch("lock");
        let live = DiskTier::open(&DiskLimits {
            directory: root.clone(),
            maximum_payload_bytes: 1024,
        })
        .expect("open");
        let again = DiskTier::open(&DiskLimits {
            directory: root.clone(),
            maximum_payload_bytes: 1024,
        });
        assert!(
            matches!(again, Err(DiskError::InUse { .. })),
            "two nodes on one dir fail at boot rather than reap each other"
        );
        drop(live);
        let reopened = DiskTier::open(&DiskLimits {
            directory: root.clone(),
            maximum_payload_bytes: 1024,
        });
        assert!(
            reopened.is_ok(),
            "joining the worker on drop releases the directory lock"
        );
        drop(reopened);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn a_dead_spill_job_does_not_create_stream_files() {
        let root = scratch("stale-job");
        let tier = DiskTier::open(&DiskLimits {
            directory: root.clone(),
            maximum_payload_bytes: 1024,
        })
        .expect("open");
        let enqueued = tier.try_enqueue(SpillJob {
            live: Weak::new(),
            stream: StreamId::new("live/camera"),
            epoch: 1,
            rendition: RenditionId(0),
            segment: SegmentId(0),
            objects: vec![SpillObject {
                kind: SpillKind::Segment,
                payload: Payload::from(vec![1, 2, 3]),
                gzip: None,
            }],
        });
        assert!(enqueued, "the bounded queue accepts a single job");
        wait_idle(&tier);
        assert!(
            !tier.shared.generation.join("streams").exists(),
            "a job whose stream is gone must not write under that stream id"
        );
        drop(tier);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn retiring_an_epoch_waits_for_its_jobs_and_leaves_siblings() {
        let root = scratch("reap-epoch");
        let tier = DiskTier::open(&DiskLimits {
            directory: root.clone(),
            maximum_payload_bytes: 1024,
        })
        .expect("open");
        let stream = StreamId::new("live/camera");
        let live = Arc::new(LiveStream::new(
            stream.clone(),
            RetentionPolicy::default(),
            Some(Arc::clone(&tier)),
        ));
        live.attach_handle(Arc::downgrade(&live));
        let epoch = live.disk_epoch();
        let sibling = epoch_dir(&tier, &stream, epoch ^ 1);
        fs::create_dir_all(&sibling).expect("sibling epoch");
        fs::write(sibling.join("keep.bin"), b"x").expect("marker");

        let enqueued = tier.try_enqueue(SpillJob {
            live: Arc::downgrade(&live),
            stream: stream.clone(),
            epoch,
            rendition: RenditionId(0),
            segment: SegmentId(0),
            objects: vec![SpillObject {
                kind: SpillKind::Segment,
                payload: Payload::from(vec![1, 2, 3]),
                gzip: None,
            }],
        });
        assert!(enqueued, "the bounded queue accepts a single job");
        tier.remove_stream(&stream, epoch);
        wait_idle(&tier);
        assert!(
            !epoch_dir(&tier, &stream, epoch).exists(),
            "the retired epoch is gone after its spill jobs finish"
        );
        assert!(
            sibling.join("keep.bin").exists(),
            "a successor epoch under the same stream id is left alone"
        );
        drop(live);
        drop(tier);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn a_failed_spill_write_is_counted() {
        let root = scratch("spill-fail");
        let tier = DiskTier::open(&DiskLimits {
            directory: root.clone(),
            maximum_payload_bytes: 1024,
        })
        .expect("open");
        let stream = StreamId::new("live/camera");
        let live = Arc::new(LiveStream::new(
            stream.clone(),
            RetentionPolicy::default(),
            Some(Arc::clone(&tier)),
        ));
        live.attach_handle(Arc::downgrade(&live));
        let epoch = live.disk_epoch();
        let epoch_path = epoch_dir(&tier, &stream, epoch);
        fs::create_dir_all(epoch_path.parent().expect("epoch parent")).expect("stream dir");
        fs::write(&epoch_path, b"not a directory").expect("block epoch path");

        let enqueued = tier.try_enqueue(SpillJob {
            live: Arc::downgrade(&live),
            stream: stream.clone(),
            epoch,
            rendition: RenditionId(0),
            segment: SegmentId(0),
            objects: vec![SpillObject {
                kind: SpillKind::Segment,
                payload: Payload::from(vec![1, 2, 3]),
                gzip: None,
            }],
        });
        assert!(enqueued, "the bounded queue accepts a single job");
        wait_idle(&tier);
        assert_eq!(
            tier.spills_failed(),
            1,
            "a write that cannot create its epoch directory is a failed spill"
        );
        drop(live);
        drop(tier);
        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn in_flight_disk_reads_wait_on_the_byte_semaphore() {
        let root = scratch("sem");
        let tier = DiskTier::open(&DiskLimits {
            directory: root.clone(),
            maximum_payload_bytes: 1024,
        })
        .expect("open");

        let path_a = fifo(&root, "a");
        let path_b = fifo(&root, "b");
        let path_c = fifo(&root, "c");
        let read_a = DiskRef {
            path: Arc::new(path_a.clone()),
            len: 16,
        };
        let read_b = DiskRef {
            path: Arc::new(path_b.clone()),
            len: 16,
        };
        let read_c = DiskRef {
            path: Arc::new(path_c.clone()),
            len: 16,
        };

        let first = {
            let tier = Arc::clone(&tier);
            tokio::spawn(async move { tier.read(&read_a).await })
        };
        let second = {
            let tier = Arc::clone(&tier);
            tokio::spawn(async move { tier.read(&read_b).await })
        };
        tokio::time::sleep(std::time::Duration::from_millis(80)).await;
        let third = {
            let tier = Arc::clone(&tier);
            tokio::spawn(async move { tier.read(&read_c).await })
        };
        tokio::time::sleep(std::time::Duration::from_millis(80)).await;
        assert!(
            !third.is_finished(),
            "the third read waits until in-flight buffers free the semaphore"
        );

        std::thread::spawn(move || {
            let _ = fs::write(&path_a, [0_u8; 16]);
        });
        first.await.expect("join").expect("read a");
        assert!(
            !third.is_finished(),
            "completing one read is not enough while the second still holds permits"
        );
        std::thread::spawn(move || {
            let _ = fs::write(&path_b, [0_u8; 16]);
            let _ = fs::write(&path_c, [0_u8; 16]);
        });
        second.await.expect("join").expect("read b");
        third.await.expect("join").expect("read c");
        drop(tier);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn hostile_stream_names_are_refused_rather_than_escaped() {
        for name in ["../etc", "/abs", "a//b", ""] {
            assert!(
                stream_relative(&StreamId::new(name)).is_err(),
                "{name} must not become a disk path"
            );
        }
        assert!(stream_relative(&StreamId::new("live/camera")).is_ok());
    }
}
