use std::{
    fs, io,
    sync::Arc,
    thread,
    time::{Duration, Instant},
};

use super::{DiskLimits, RetentionPolicy, StoreLimits, StreamStore, disk::DiskTier};

pub fn limits() -> StoreLimits {
    StoreLimits {
        maximum_streams: 8,
        independent_segments: false,
        memory: None,
        retention: RetentionPolicy {
            // Six six-second segments. Pinned rather than taken from the
            // default so these fixtures describe a window of a known size,
            // independent of whatever `retain` a shipped file chooses.
            retain: Duration::from_secs(36).into(),
            ..RetentionPolicy::default()
        },
        disk: None,
    }
}

pub fn scratch_disk(name: &str, maximum_payload_bytes: usize) -> io::Result<DiskLimits> {
    let directory = std::env::temp_dir().join(format!("rushls-{name}-{}", uuid::Uuid::new_v4()));
    fs::create_dir(&directory)?;
    Ok(DiskLimits {
        directory,
        maximum_payload_bytes,
    })
}

pub fn spill_limits(disk: &DiskLimits, retain: Duration, memory_bytes: usize) -> StoreLimits {
    let mut settings = limits();
    settings.retention.retain = retain.into();
    settings.retention.maximum_payload_bytes = memory_bytes;
    settings.disk = Some(disk.clone());
    settings
}

pub fn remove_disk(store: StreamStore, scratch: DiskLimits) -> io::Result<()> {
    let disk = Arc::clone(store.disk().expect("disk configured"));
    drop(store);
    close_disk(disk);
    fs::remove_dir_all(scratch.directory)
}

pub fn close_disk(disk: Arc<DiskTier>) {
    // A spill worker can briefly retain the last live stream. Keep the tier
    // here until it releases that stream, then join it before deleting locked
    // files (Windows correctly refuses deletion while they are still owned).
    wait_until("disk worker releases stream", || {
        Arc::strong_count(&disk) == 1
    });
    drop(disk);
}

pub fn wait_until(description: &str, ready: impl Fn() -> bool) {
    let start = Instant::now();
    while !ready() {
        assert!(
            start.elapsed() < Duration::from_secs(2),
            "timed out waiting for {description}"
        );
        thread::sleep(Duration::from_millis(2));
    }
}
