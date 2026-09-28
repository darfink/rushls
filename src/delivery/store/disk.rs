//! Overflow tier: completed parents the memory cap cannot hold.
//!
//! New media still lands in RAM. This module only writes what the store has
//! already decided to spill, and only reads what a viewer asked for by URI.
//! Playlist projection never comes here. A restart does not rehydrate the
//! catalog: generation directories are process-lifetime. The directory lock
//! refuses a second process, so cutover is stop-then-start. Reap on open
//! removes generations whose `owner.lock` is no longer held. Ownership does
//! not depend on process IDs. Unmarked directories and legacy PID-marked
//! generations are left untouched.

use std::{
    collections::HashMap,
    fs::{self, File, OpenOptions},
    io::{self, Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicU64, AtomicUsize, Ordering},
        mpsc::{self, Receiver, SyncSender, TrySendError},
    },
    thread::{self, JoinHandle},
};

use parking_lot::Mutex;
use tokio::sync::{Notify, Semaphore};

use crate::domain::{Payload, RenditionId, StreamId};

use super::{PartId, SegmentId};

/// Maximum queued writes; publishers wait when this worker is saturated.
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
    /// Byte offset inside a completed segment file (zero for whole files).
    pub offset: u64,
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

    pub fn bytes(&self) -> Option<bytes::Bytes> {
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
    progress: Notify,
    #[cfg(test)]
    write_gate: Mutex<()>,
    spills_failed: AtomicU64,
    operations: crate::observe::OperationMeters,
    /// In-flight and retired epochs. A successor may reuse the public stream
    /// id, so retirement deletes one epoch and waits for its jobs first.
    epochs: Mutex<HashMap<(StreamId, u64), EpochJobs>>,
    read_bytes: Arc<Semaphore>,
    /// Exclusive lock on `dir`. Dropped with the generation so a peer cannot
    /// reap files while a spill is still finishing.
    _lock: File,
    owner: Option<File>,
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
        let owner_path = generation.join("owner.lock");
        let owner = OpenOptions::new()
            .create_new(true)
            .read(true)
            .write(true)
            .open(&owner_path)
            .map_err(|source| DiskError::Create {
                path: owner_path.clone(),
                source,
            })?;
        owner.lock().map_err(|source| DiskError::Create {
            path: owner_path,
            source,
        })?;

        let (jobs, rx) = mpsc::sync_channel(SPILL_QUEUE_BOUND);
        let shared = Arc::new(DiskShared {
            generation: generation.clone(),
            maximum_payload_bytes: limits.maximum_payload_bytes,
            pending: AtomicUsize::new(0),
            progress: Notify::new(),
            #[cfg(test)]
            write_gate: Mutex::new(()),
            spills_failed: AtomicU64::new(0),
            operations: crate::observe::OperationMeters::default(),
            epochs: Mutex::new(HashMap::new()),
            read_bytes: Arc::new(Semaphore::new(MAX_IN_FLIGHT_READ_BYTES as usize)),
            _lock: lock,
            owner: Some(owner),
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

    /// Register before checking capacity so a completion cannot be missed.
    pub fn progress(&self) -> &Notify {
        &self.shared.progress
    }

    #[cfg(test)]
    pub fn pause_writes(&self) -> parking_lot::MutexGuard<'_, ()> {
        self.shared.write_gate.lock()
    }

    pub fn maximum_payload_bytes(&self) -> usize {
        self.shared.maximum_payload_bytes
    }

    pub fn queue_is_full(&self) -> bool {
        self.spill_pending() >= SPILL_QUEUE_BOUND
    }

    pub fn operations(&self) -> crate::observe::OperationMeters {
        self.shared.operations.clone()
    }

    pub fn spill_capacity() -> usize {
        SPILL_QUEUE_BOUND
    }

    pub fn spill_pending(&self) -> usize {
        self.shared.pending.load(Ordering::Acquire)
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
                self.shared.note_finished(&job.stream, job.epoch);
                self.shared.pending.fetch_sub(1, Ordering::Relaxed);
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
        let offset = disk.offset;
        let len = disk.len;
        let result = tokio::task::spawn_blocking(move || {
            let mut file = File::open(&*path)?;
            if offset != 0 {
                file.seek(SeekFrom::Start(offset))?;
            }
            let mut bytes = vec![0; len];
            file.read_exact(&mut bytes)?;
            Ok::<_, io::Error>(bytes)
        })
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
        drop(self.owner.take());
        let _ = fs::remove_dir_all(&self.generation);
    }
}

fn spill_loop(shared: &DiskShared, rx: &Receiver<SpillJob>) {
    while let Ok(job) = rx.recv() {
        // Upgrade before any write: a retired stream's public id may already
        // belong to a new LiveStream, and those paths must not be reused.
        let live = job.live.upgrade();
        if let Some(live) = &live {
            if shared.is_reaping(&job.stream, job.epoch) {
                live.abort_spill(job.rendition, job.segment);
            } else {
                let measurement = shared
                    .operations
                    .start(crate::observe::Operation::DiskWrite);
                let result = write_job(shared, &job);
                measurement.finish(if result.is_ok() {
                    crate::observe::OperationOutcome::Completed
                } else {
                    crate::observe::OperationOutcome::Error
                });
                match result {
                    Ok(outcome) => live.finish_spill(&outcome),
                    Err(error) => {
                        shared.spills_failed.fetch_add(1, Ordering::Relaxed);
                        tracing::warn!(
                            stream = %job.stream,
                            error = %error,
                            "disk spill failed; media stays in memory"
                        );
                        live.fail_spill(job.rendition, job.segment);
                    }
                }
            }
        }
        shared.note_finished(&job.stream, job.epoch);
        // Only after any deferred epoch reap: a zero count must mean every
        // effect of the job, including removing a retired epoch, is done.
        shared.pending.fetch_sub(1, Ordering::Release);
        // Release payload references before admitting more publisher bytes.
        drop(job);
        if let Some(live) = live {
            live.maybe_spill();
        }
        shared.progress.notify_waiters();
    }
}

fn write_job(shared: &DiskShared, job: &SpillJob) -> Result<SpillOutcome, DiskError> {
    #[cfg(test)]
    let _gate = shared.write_gate.lock();
    let path = shared.object_path(
        &job.stream,
        job.epoch,
        job.rendition,
        &format!("segment-{}.bin", job.segment.0),
    )?;
    // Sequential slices avoid allocating a second, segment-sized buffer.
    write_atomic_slices(
        &path,
        job.objects.iter().map(|object| object.payload.as_bytes()),
    )?;
    let gzip_path = path.with_extension("bin.gz");
    let has_gzip = job.objects.iter().any(|object| object.gzip.is_some());
    if has_gzip
        && let Err(error) = write_atomic_slices(
            &gzip_path,
            job.objects
                .iter()
                .filter_map(|object| object.gzip.as_ref().map(Payload::as_bytes)),
        )
    {
        let _ = fs::remove_file(&path);
        return Err(error);
    }
    let path = Arc::new(path);
    let gzip_path = Arc::new(gzip_path);
    let mut offset = 0;
    let mut gzip_offset = 0;
    let objects = job
        .objects
        .iter()
        .map(|object| {
            let payload = DiskRef {
                path: Arc::clone(&path),
                offset,
                len: object.payload.len(),
            };
            offset += object.payload.len() as u64;
            let gzip = object.gzip.as_ref().map(|gzip| {
                let locator = DiskRef {
                    path: Arc::clone(&gzip_path),
                    offset: gzip_offset,
                    len: gzip.len(),
                };
                gzip_offset += gzip.len() as u64;
                locator
            });
            SpilledObject {
                kind: object.kind,
                payload,
                gzip,
            }
        })
        .collect();
    Ok(SpillOutcome {
        rendition: job.rendition,
        segment: job.segment,
        objects,
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

fn write_atomic_slices<'a>(
    path: &Path,
    slices: impl Iterator<Item = &'a [u8]>,
) -> Result<(), DiskError> {
    let tmp = path.with_extension("tmp");
    let result = write_atomic_inner(path, &tmp, slices);
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
        let _ = fs::remove_file(path);
    }
    result
}

fn write_atomic_inner<'a>(
    path: &Path,
    tmp: &Path,
    slices: impl Iterator<Item = &'a [u8]>,
) -> Result<(), DiskError> {
    let mut options = OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o644);
    }
    let mut file = options.open(tmp).map_err(|source| DiskError::Write {
        path: tmp.to_path_buf(),
        source,
    })?;
    for bytes in slices {
        file.write_all(bytes).map_err(|source| DiskError::Write {
            path: tmp.to_path_buf(),
            source,
        })?;
    }
    file.sync_all().map_err(|source| DiskError::Write {
        path: tmp.to_path_buf(),
        source,
    })?;
    fs::rename(tmp, path).map_err(|source| DiskError::Write {
        path: path.to_path_buf(),
        source,
    })?;
    if let Some(parent) = path.parent() {
        let dir = cap_std::fs::Dir::open_ambient_dir(parent, cap_std::ambient_authority())
            .map_err(|source| DiskError::Write {
                path: parent.to_path_buf(),
                source,
            })?;
        crate::delivery::filesystem::sync_directory(&dir).map_err(|source| DiskError::Write {
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
    match file.try_lock() {
        Ok(()) => {}
        Err(std::fs::TryLockError::WouldBlock) => return Err(DiskError::InUse { path }),
        Err(std::fs::TryLockError::Error(source)) => {
            return Err(DiskError::Create { path, source });
        }
    }
    Ok(file)
}

fn unique_generation(root: &Path) -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |duration| duration.as_nanos());
    root.join(format!("{}-{nanos}", std::process::id()))
}

/// Called only while holding the root lock. A generation without an owner
/// marker is never ours to delete; errors and held locks conservatively keep it.
fn reap_dead_generations(root: &Path) {
    let Ok(entries) = fs::read_dir(root) else {
        return;
    };
    for entry in entries.flatten() {
        if !entry.file_type().is_ok_and(|kind| kind.is_dir()) {
            continue;
        }
        let path = entry.path();
        if !fs::symlink_metadata(path.join("owner.lock")).is_ok_and(|metadata| metadata.is_file()) {
            continue;
        }
        let Ok(owner) = OpenOptions::new()
            .read(true)
            .write(true)
            .open(path.join("owner.lock"))
        else {
            continue;
        };
        if owner.try_lock().is_err() {
            continue;
        }
        // Windows cannot remove an open locked owner file. The root lock still
        // excludes another Rushls opener while we release this stale handle.
        drop(owner);
        let _ = fs::remove_dir_all(path);
    }
}

fn stream_relative(stream: &StreamId) -> Result<PathBuf, DiskError> {
    let mut path = PathBuf::from("streams");
    let mut any = false;
    for segment in stream.as_str().split('/') {
        if !crate::delivery::filesystem::safe_component(std::ffi::OsStr::new(segment)) {
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

    #[cfg(unix)]
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

    /// This helper is also an ordinary no-op test unless invoked by the parent
    /// with a private directory. Exiting bypasses Drop to model a crashed writer.
    #[test]
    fn ownership_child() -> Result<(), Box<dyn std::error::Error>> {
        let Some(root) = std::env::var_os("RUSHLS_TEST_OWNER_DIRECTORY") else {
            return Ok(());
        };
        let root = PathBuf::from(root);
        let tier = DiskTier::open(&DiskLimits {
            directory: root,
            maximum_payload_bytes: 1024,
        });
        if std::env::var_os("RUSHLS_TEST_OWNER_BUSY").is_some() {
            assert!(matches!(tier, Err(DiskError::InUse { .. })));
            return Ok(());
        }
        let tier = tier?;
        fs::write(tier.shared.generation.join("crash-evidence"), b"orphan")?;
        std::process::exit(0);
    }

    #[test]
    fn process_locks_reject_live_writers_and_reap_crashed_writers()
    -> Result<(), Box<dyn std::error::Error>> {
        let root = scratch("process-ownership");
        let limits = DiskLimits {
            directory: root.clone(),
            maximum_payload_bytes: 1024,
        };
        let tier = DiskTier::open(&limits)?;
        let run_child = |busy: bool| -> std::io::Result<std::process::ExitStatus> {
            let mut child = std::process::Command::new(std::env::current_exe()?);
            child
                .args([
                    "--exact",
                    "delivery::store::disk::tests::ownership_child",
                    "--nocapture",
                ])
                .env("RUSHLS_TEST_OWNER_DIRECTORY", &root);
            if busy {
                child.env("RUSHLS_TEST_OWNER_BUSY", "1");
            } else {
                child.env_remove("RUSHLS_TEST_OWNER_BUSY");
            }
            child.status()
        };
        assert!(run_child(true)?.success());
        drop(tier);
        assert!(run_child(false)?.success());
        let stale = fs::read_dir(&root)?
            .filter_map(std::result::Result::ok)
            .map(|entry| entry.path())
            .find(|path| path.join("crash-evidence").exists())
            .ok_or("child left no crash evidence")?;
        let reopened = DiskTier::open(&limits)?;
        assert!(
            !stale.exists(),
            "an exited process must release its generation lock"
        );
        drop(reopened);
        fs::remove_dir_all(root)?;
        Ok(())
    }

    #[test]
    fn reap_removes_a_dead_generation_and_leaves_a_live_one() {
        let root = scratch("reap");
        let dead = root.join("dead-gen");
        fs::create_dir_all(&dead).unwrap();
        fs::write(dead.join("owner.lock"), "").unwrap();
        fs::write(dead.join("orphan.bin"), b"x").unwrap();

        let peer = root.join("peer-gen");
        fs::create_dir_all(&peer).unwrap();
        let peer_owner = OpenOptions::new()
            .create_new(true)
            .read(true)
            .write(true)
            .open(peer.join("owner.lock"))
            .unwrap();
        peer_owner.lock().unwrap();

        let unrelated = root.join("not-a-generation");
        fs::create_dir_all(&unrelated).unwrap();
        fs::write(unrelated.join("keep.bin"), b"x").unwrap();

        let live = DiskTier::open(&DiskLimits {
            directory: root.clone(),
            maximum_payload_bytes: 1024,
        })
        .expect("open");

        assert!(!dead.exists(), "an unheld owner lock permits crash cleanup");
        assert!(
            peer.exists(),
            "a generation with a held owner lock is left alone"
        );
        assert!(
            unrelated.exists(),
            "a directory without owner.lock is not a generation and is left alone"
        );
        assert!(live.shared.generation.exists());
        assert!(
            live.shared.generation.join("owner.lock").exists(),
            "the live generation keeps its lock file"
        );
        drop(live);
        drop(peer_owner);
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

    #[tokio::test]
    async fn a_failed_spill_write_is_counted() {
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
        assert_eq!(
            live.ready(0).await,
            Err(crate::delivery::store::StoreWriteError::DiskSpillFailed)
        );
        drop(live);
        drop(tier);
        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn disk_reads_wait_for_capacity_on_every_platform()
    -> Result<(), Box<dyn std::error::Error>> {
        let root = scratch("portable-read-budget");
        let tier = DiskTier::open(&DiskLimits {
            directory: root.clone(),
            maximum_payload_bytes: 1024,
        })?;
        let path = root.join("payload.bin");
        fs::write(&path, b"payload")?;
        let held = Arc::clone(&tier.shared.read_bytes)
            .acquire_many_owned(MAX_IN_FLIGHT_READ_BYTES)
            .await?;
        let reader = Arc::clone(&tier);
        let mut pending = tokio::spawn(async move {
            reader
                .read(&DiskRef {
                    path: Arc::new(path),
                    offset: 0,
                    len: 7,
                })
                .await
        });
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(20), &mut pending)
                .await
                .is_err()
        );
        drop(held);
        let payload = tokio::time::timeout(std::time::Duration::from_secs(5), pending).await???;
        assert_eq!(payload.as_bytes(), b"payload");
        drop(tier);
        fs::remove_dir_all(root)?;
        Ok(())
    }

    #[cfg(unix)]
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
            offset: 0,
            path: Arc::new(path_a.clone()),
            len: 16,
        };
        let read_b = DiskRef {
            offset: 0,
            path: Arc::new(path_b.clone()),
            len: 16,
        };
        let read_c = DiskRef {
            offset: 0,
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
