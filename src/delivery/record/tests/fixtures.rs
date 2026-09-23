use super::super::*;
use crate::{
    delivery::{
        hls::{StoreLimits, StorePublisherFactory, fixtures as hls},
        store::StreamStore,
    },
    domain::SessionId,
    mux::{PackagedSegmentCompletion, PackagingRenditionId},
    observe::{EventObserver, SessionEvent},
};
use parking_lot::Mutex;

pub struct Temp(pub PathBuf);
impl Temp {
    pub fn new() -> Self {
        Self(std::env::temp_dir().join(format!("rushls-record-{}", Uuid::now_v7())))
    }
    pub fn config(&self) -> Config {
        Config {
            dir: self.0.clone(),
            pattern: "{stream}/{publication}/{rendition}_{segment}.mp4".into(),
            queue_capacity: 8,
            maximum_pending_bytes: 1024 * 1024,
        }
    }
    pub fn files(&self) -> std::io::Result<Vec<PathBuf>> {
        fn visit(dir: &std::path::Path, out: &mut Vec<PathBuf>) -> std::io::Result<()> {
            for item in std::fs::read_dir(dir)? {
                let path = item?.path();
                if path.is_dir() {
                    visit(&path, out)?;
                } else {
                    out.push(path);
                }
            }
            Ok(())
        }
        let mut files = Vec::new();
        visit(&self.0, &mut files)?;
        files.sort();
        Ok(files)
    }
}
impl Drop for Temp {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
#[derive(Default)]
pub struct Log(pub Mutex<Vec<NodeEvent>>);
impl EventObserver for Log {
    fn observe(&self, _: SessionId, _: SessionEvent) {}
    fn observe_node(&self, event: NodeEvent) {
        self.0.lock().push(event);
    }
}
pub fn factory(recorder: Recorder) -> RecordingFactory {
    RecordingFactory {
        inner: Arc::new(StorePublisherFactory::new(StreamStore::new(
            StoreLimits::default(),
        ))),
        recorder,
    }
}
pub fn presentation() -> Arc<PackagedPresentation> {
    hls::presentation(vec![hls::audio(0)])
}
pub fn complete(id: u64, start: i64, duration: u64) -> PackagedMedia {
    PackagedMedia::SegmentCompleted(PackagedSegmentCompletion {
        rendition_id: PackagingRenditionId(0),
        packaging_segment_id: PackagingSegmentId(id),
        media_start: start,
        duration,
    })
}

/// A Windows junction exercises reparse-point confinement without requiring
/// Developer Mode or the privilege needed to create symbolic links.
pub fn directory_link(target: &std::path::Path, link: &std::path::Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(target, link)
    }
    #[cfg(windows)]
    {
        junction::create(target, link)
    }
}
