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
    collections::{HashMap, HashSet},
    fs::{self, File, OpenOptions},
    io::{self, Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicU64, AtomicUsize, Ordering},
        mpsc::{self, Receiver, Sender},
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
    jobs: Option<Sender<DiskWork>>,
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
    /// Only directories for accepted spills can enqueue removal. Claiming a
    /// directory here coalesces all of its segment files and gzip sidecars.
    directories: Mutex<HashMap<u64, HashSet<RenditionId>>>,
    read_bytes: Arc<Semaphore>,
    /// Exclusive lock on `dir`. Dropped with the generation so a peer cannot
    /// reap files while a spill is still finishing.
    _lock: File,
    owner: Option<File>,
}

enum DiskWork {
    Spill(SpillJob),
    Remove {
        epoch: u64,
        rendition: Option<RenditionId>,
    },
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
    /// Diagnostic label for failed writes.
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

        let (jobs, rx) = mpsc::channel();
        let shared = Arc::new(DiskShared {
            generation: generation.clone(),
            maximum_payload_bytes: limits.maximum_payload_bytes,
            pending: AtomicUsize::new(0),
            progress: Notify::new(),
            #[cfg(test)]
            write_gate: Mutex::new(()),
            spills_failed: AtomicU64::new(0),
            operations: crate::observe::OperationMeters::default(),
            directories: Mutex::new(HashMap::new()),
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
        // Serialize admission across streams. The FIFO also accepts one small
        // cleanup request per previously admitted directory, even when the
        // payload queue is full and the publisher holds the stream lock.
        let mut directories = self.shared.directories.lock();
        if self.queue_is_full() {
            return false;
        }
        let epoch = job.epoch;
        let rendition = job.rendition;
        self.shared.pending.fetch_add(1, Ordering::Relaxed);
        if jobs.send(DiskWork::Spill(job)).is_err() {
            self.shared.pending.fetch_sub(1, Ordering::Relaxed);
            return false;
        }
        directories.entry(epoch).or_default().insert(rendition);
        true
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

    /// Pruning holds the same stream lock as spill admission. Once pruned,
    /// this durable rendition cannot enqueue another write behind its removal.
    pub fn remove_rendition(&self, epoch: u64, rendition: RenditionId) {
        let mut directories = self.shared.directories.lock();
        if directories
            .get_mut(&epoch)
            .is_some_and(|ids| ids.remove(&rendition))
        {
            self.enqueue_removal(epoch, Some(rendition));
        }
    }

    /// Removes only this retired epoch, after its previously admitted work.
    pub fn remove_stream(&self, epoch: u64) {
        if self.shared.directories.lock().remove(&epoch).is_some() {
            self.enqueue_removal(epoch, None);
        }
    }

    fn enqueue_removal(&self, epoch: u64, rendition: Option<RenditionId>) {
        if let Some(jobs) = &self.jobs {
            let _ = jobs.send(DiskWork::Remove { epoch, rendition });
        }
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

fn spill_loop(shared: &DiskShared, rx: &Receiver<DiskWork>) {
    while let Ok(work) = rx.recv() {
        let job = match work {
            DiskWork::Spill(job) => job,
            DiskWork::Remove { epoch, rendition } => {
                shared.remove_directory(epoch, rendition);
                continue;
            }
        };
        // Upgrade before any write: a retired stream's public id may already
        // belong to a new LiveStream, and those paths must not be reused.
        let live = job.live.upgrade();
        if let Some(live) = &live {
            if shared.owns_directory(job.epoch, job.rendition) {
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
                    Ok(outcome) => {
                        if !live.finish_spill(&outcome) {
                            forget_spilled(&outcome.objects);
                        }
                    }
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
            } else {
                live.abort_spill(job.rendition, job.segment);
            }
        }
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
    let path = shared.segment_path(job.epoch, job.rendition, job.segment)?;
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
    fn owns_directory(&self, epoch: u64, rendition: RenditionId) -> bool {
        self.directories
            .lock()
            .get(&epoch)
            .is_some_and(|ids| ids.contains(&rendition))
    }

    // Epochs are unique within a generation. Keeping them as siblings means
    // retiring one stream can never traverse another stream's spill files.
    fn epoch_dir(&self, epoch: u64) -> PathBuf {
        self.generation.join(format!("epoch-{epoch}"))
    }

    fn rendition_dir(&self, epoch: u64, rendition: RenditionId) -> PathBuf {
        self.epoch_dir(epoch)
            .join(format!("rendition-{}", rendition.0))
    }

    fn remove_directory(&self, epoch: u64, rendition: Option<RenditionId>) {
        #[cfg(test)]
        let _gate = self.write_gate.lock();
        let path = match rendition {
            Some(rendition) => self.rendition_dir(epoch, rendition),
            None => self.epoch_dir(epoch),
        };
        let _ = fs::remove_dir_all(path);
    }

    fn segment_path(
        &self,
        epoch: u64,
        rendition: RenditionId,
        segment: SegmentId,
    ) -> Result<PathBuf, DiskError> {
        let mut path = self.rendition_dir(epoch, rendition);
        fs::create_dir_all(&path).map_err(|source| DiskError::Create {
            path: path.clone(),
            source,
        })?;
        path.push(format!("segment-{}.bin", segment.0));
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

fn forget_spilled(objects: &[SpilledObject]) {
    // Every part of this job refers to a range in the same file or sidecar.
    if let Some(object) = objects.first() {
        let _ = fs::remove_file(&*object.payload.path);
    }
    if let Some(gzip) = objects.iter().find_map(|object| object.gzip.as_ref()) {
        let _ = fs::remove_file(&*gzip.path);
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

#[cfg(test)]
mod tests {
    use std::{
        sync::{Arc, Weak},
        time::{Duration, Instant},
    };

    use crate::{
        delivery::hls::fixtures::stream_id,
        domain::{Payload, RenditionId, StreamId},
    };

    use super::{
        super::{
            LiveStream, RetentionPolicy, SegmentId,
            fixtures::{close_disk, scratch_disk, wait_until},
        },
        *,
    };

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
        wait_until("the spill worker", || tier.spill_pending() == 0);
    }

    fn live_stream(tier: &Arc<DiskTier>) -> Arc<LiveStream> {
        let live = Arc::new(LiveStream::new(
            stream_id(),
            RetentionPolicy::default(),
            Some(Arc::clone(tier)),
        ));
        live.attach_handle(Arc::downgrade(&live));
        live
    }

    fn segment_job(live: Option<&Arc<LiveStream>>) -> SpillJob {
        SpillJob {
            live: live.map_or_else(Weak::new, Arc::downgrade),
            stream: stream_id(),
            epoch: live.map_or(1, |live| live.disk_epoch()),
            rendition: RenditionId(0),
            segment: SegmentId(0),
            objects: vec![SpillObject {
                kind: SpillKind::Segment,
                payload: Payload::from(vec![1, 2, 3]),
                gzip: None,
            }],
        }
    }

    /// Opens a directory whose previous owner was just dropped.
    ///
    /// Drop releases the lock synchronously, but another test in this binary
    /// may be spawning a child at that moment. The spawn briefly duplicates
    /// every descriptor, and a `flock` lives until its last duplicate closes,
    /// so the lock can outlast our drop by the length of that spawn. Only the
    /// test binary spawns processes; production never shares the window.
    fn open_after_release(limits: &DiskLimits) -> Result<Arc<DiskTier>, DiskError> {
        let start = Instant::now();
        loop {
            match DiskTier::open(limits) {
                Err(DiskError::InUse { .. }) if start.elapsed() < Duration::from_secs(2) => {
                    thread::sleep(Duration::from_millis(5));
                }
                result => return result,
            }
        }
    }

    /// This helper is also an ordinary no-op test unless invoked by the parent
    /// with a private directory. Exiting bypasses Drop to model a crashed writer.
    #[test]
    fn ownership_child() -> Result<(), Box<dyn std::error::Error>> {
        let Some(root) = std::env::var_os("RUSHLS_TEST_OWNER_DIRECTORY") else {
            return Ok(());
        };
        let root = PathBuf::from(root);
        let limits = DiskLimits {
            directory: root,
            maximum_payload_bytes: 1024,
        };
        if std::env::var_os("RUSHLS_TEST_OWNER_BUSY").is_some() {
            assert!(matches!(
                DiskTier::open(&limits),
                Err(DiskError::InUse { .. })
            ));
            return Ok(());
        }
        // The parent has just dropped its tier.
        let tier = open_after_release(&limits)?;
        fs::write(tier.shared.generation.join("crash-evidence"), b"orphan")?;
        std::process::exit(0);
    }

    #[test]
    fn process_locks_reject_live_writers_and_reap_crashed_writers()
    -> Result<(), Box<dyn std::error::Error>> {
        let limits = scratch_disk("process-ownership", 1024)?;
        let root = &limits.directory;
        let tier = DiskTier::open(&limits)?;
        let run_child = |busy: bool| -> std::io::Result<std::process::ExitStatus> {
            let mut child = std::process::Command::new(std::env::current_exe()?);
            child
                .args([
                    "--exact",
                    "delivery::store::disk::tests::ownership_child",
                    "--nocapture",
                ])
                .env("RUSHLS_TEST_OWNER_DIRECTORY", root);
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
        let stale = fs::read_dir(root)?
            .filter_map(std::result::Result::ok)
            .map(|entry| entry.path())
            .find(|path| path.join("crash-evidence").exists())
            .ok_or("child left no crash evidence")?;
        let reopened = open_after_release(&limits)?;
        assert!(
            !stale.exists(),
            "an exited process must release its generation lock"
        );
        drop(reopened);
        fs::remove_dir_all(root)?;
        Ok(())
    }

    #[test]
    fn reap_removes_a_dead_generation_and_leaves_a_live_one()
    -> Result<(), Box<dyn std::error::Error>> {
        let limits = scratch_disk("reap", 1024)?;
        let root = &limits.directory;
        let dead = root.join("dead-gen");
        fs::create_dir_all(&dead)?;
        fs::write(dead.join("owner.lock"), "")?;
        fs::write(dead.join("orphan.bin"), b"x")?;

        let peer = root.join("peer-gen");
        fs::create_dir_all(&peer)?;
        let peer_owner = OpenOptions::new()
            .create_new(true)
            .read(true)
            .write(true)
            .open(peer.join("owner.lock"))?;
        peer_owner.lock()?;

        let unrelated = root.join("not-a-generation");
        fs::create_dir_all(&unrelated)?;
        fs::write(unrelated.join("keep.bin"), b"x")?;

        let live = DiskTier::open(&limits)?;

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
        fs::remove_dir_all(root)?;
        Ok(())
    }

    #[test]
    fn a_second_process_cannot_share_the_directory() -> Result<(), Box<dyn std::error::Error>> {
        let limits = scratch_disk("lock", 1024)?;
        let live = DiskTier::open(&limits)?;
        let again = DiskTier::open(&limits);
        assert!(
            matches!(again, Err(DiskError::InUse { .. })),
            "two nodes on one dir fail at boot rather than reap each other"
        );
        drop(live);
        let reopened = open_after_release(&limits);
        assert!(
            reopened.is_ok(),
            "joining the worker on drop releases the directory lock: {:?}",
            reopened.as_ref().err()
        );
        drop(reopened);
        fs::remove_dir_all(limits.directory)?;
        Ok(())
    }

    #[test]
    fn a_dead_spill_job_does_not_create_stream_files() -> Result<(), Box<dyn std::error::Error>> {
        let limits = scratch_disk("stale-job", 1024)?;
        let tier = DiskTier::open(&limits)?;
        let enqueued = tier.try_enqueue(segment_job(None));
        assert!(enqueued, "the bounded queue accepts a single job");
        wait_idle(&tier);
        assert!(
            !tier.shared.epoch_dir(1).exists(),
            "a job whose stream is gone must not create an epoch directory"
        );
        drop(tier);
        fs::remove_dir_all(limits.directory)?;
        Ok(())
    }

    #[test]
    fn retiring_an_epoch_waits_for_its_jobs_and_leaves_siblings()
    -> Result<(), Box<dyn std::error::Error>> {
        let limits = scratch_disk("reap-epoch", 1024)?;
        let tier = DiskTier::open(&limits)?;
        let live = live_stream(&tier);
        let epoch = live.disk_epoch();
        let sibling = tier.shared.epoch_dir(epoch ^ 1);
        fs::create_dir_all(&sibling)?;
        fs::write(sibling.join("keep.bin"), b"x")?;
        let retired = tier.shared.epoch_dir(epoch);
        fs::create_dir_all(&retired)?;
        fs::write(retired.join("retired.bin"), b"x")?;

        let enqueued = tier.try_enqueue(segment_job(Some(&live)));
        assert!(enqueued, "the bounded queue accepts a single job");
        tier.remove_stream(epoch);
        wait_until("the worker removes the retired epoch", || !retired.exists());
        assert_eq!(tier.spill_pending(), 0);
        assert!(
            sibling.join("keep.bin").exists(),
            "a successor epoch under the same stream id is left alone"
        );
        drop(live);
        close_disk(tier);
        fs::remove_dir_all(limits.directory)?;
        Ok(())
    }

    #[tokio::test]
    async fn a_failed_spill_write_is_counted() -> Result<(), Box<dyn std::error::Error>> {
        let limits = scratch_disk("spill-fail", 1024)?;
        let tier = DiskTier::open(&limits)?;
        let live = live_stream(&tier);
        let (publication, _) = live.attach(&crate::delivery::hls::fixtures::presentation(vec![
            crate::delivery::hls::fixtures::video(0),
        ]));
        let epoch_path = tier.shared.epoch_dir(live.disk_epoch());
        fs::write(&epoch_path, b"not a directory")?;

        let enqueued = tier.try_enqueue(segment_job(Some(&live)));
        assert!(enqueued, "the bounded queue accepts a single job");
        wait_idle(&tier);
        assert_eq!(
            tier.spills_failed(),
            1,
            "a write that cannot create its epoch directory is a failed spill"
        );
        assert_eq!(
            live.ready(publication).await,
            Err(crate::delivery::store::StoreWriteError::DiskSpillFailed)
        );
        drop(live);
        close_disk(tier);
        fs::remove_dir_all(limits.directory)?;
        Ok(())
    }

    #[tokio::test]
    async fn disk_reads_wait_for_capacity_on_every_platform()
    -> Result<(), Box<dyn std::error::Error>> {
        let limits = scratch_disk("portable-read-budget", 1024)?;
        let root = &limits.directory;
        let tier = DiskTier::open(&limits)?;
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

    #[test]
    fn a_failed_gzip_spill_removes_the_plain_payload_and_temporary_file()
    -> Result<(), Box<dyn std::error::Error>> {
        let limits = scratch_disk("gzip-fail", 1024)?;
        let tier = DiskTier::open(&limits)?;
        let mut job = segment_job(None);
        job.segment = SegmentId(1);
        job.objects[0].payload = Payload::from(vec![1; 10]);
        job.objects[0].gzip = Some(Payload::from(vec![2; 5]));
        let path = tier
            .shared
            .segment_path(job.epoch, job.rendition, job.segment)?;
        let gzip_path = path.with_extension("bin.gz");
        // Fail the sidecar rename after the plain payload has been written.
        fs::create_dir(&gzip_path)?;
        assert!(matches!(
            write_job(&tier.shared, &job),
            Err(DiskError::Write { .. })
        ));
        assert!(!path.exists());
        assert!(!path.with_extension("tmp").exists());
        assert!(!gzip_path.with_extension("tmp").exists());
        drop(tier);
        fs::remove_dir_all(limits.directory)?;
        Ok(())
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn in_flight_disk_reads_wait_on_the_byte_semaphore()
    -> Result<(), Box<dyn std::error::Error>> {
        let limits = scratch_disk("sem", 1024)?;
        let root = &limits.directory;
        let tier = DiskTier::open(&limits)?;

        let path_a = fifo(root, "a");
        let path_b = fifo(root, "b");
        let path_c = fifo(root, "c");
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
        first.await??;
        assert!(
            !third.is_finished(),
            "completing one read is not enough while the second still holds permits"
        );
        std::thread::spawn(move || {
            let _ = fs::write(&path_b, [0_u8; 16]);
            let _ = fs::write(&path_c, [0_u8; 16]);
        });
        second.await??;
        third.await??;
        drop(tier);
        fs::remove_dir_all(root)?;
        Ok(())
    }

    #[test]
    fn spill_paths_are_independent_of_public_stream_names() -> Result<(), Box<dyn std::error::Error>>
    {
        let limits = scratch_disk("spill-path", 1024)?;
        let tier = DiskTier::open(&limits)?;
        let mut job = segment_job(None);
        let expected = tier
            .shared
            .rendition_dir(job.epoch, job.rendition)
            .join("segment-0.bin");
        for name in ["live/camera", "../etc", "/abs", "a//b", ""] {
            job.stream = StreamId::new(name);
            let outcome = write_job(&tier.shared, &job)?;
            assert_eq!(outcome.objects[0].payload.path.as_ref(), &expected);
        }
        drop(tier);
        fs::remove_dir_all(limits.directory)?;
        Ok(())
    }
}
