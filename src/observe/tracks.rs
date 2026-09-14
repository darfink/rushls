//! Per-input-track progress, bounded by the validated presentation.
use crate::domain::{MediaKind, Timebase, TrackId};
use parking_lot::Mutex;
use std::{collections::BTreeMap, sync::Arc, time::SystemTime};

pub fn timestamp() -> f64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs_f64()
}

#[derive(Clone, Debug)]
pub struct TrackSnapshot {
    pub id: TrackId,
    pub kind: MediaKind,
    pub source_timestamp: Option<f64>,
    pub normalized_timestamp: Option<f64>,
    pub source_packets: u64,
    pub source_bytes: u64,
    pub normalized_samples: u64,
    pub normalized_seconds: f64,
    pub normalized_end: Option<f64>,
    timebase: Timebase,
    origin: i64,
}
#[derive(Clone, Debug, Default)]
pub struct TrackMeters(Arc<Mutex<BTreeMap<TrackId, TrackSnapshot>>>);
impl TrackMeters {
    pub fn register(&self, tracks: impl Iterator<Item = (TrackId, MediaKind, Timebase, i64)>) {
        let mut state = self.0.lock();
        for (id, kind, timebase, origin) in tracks {
            state.insert(
                id,
                TrackSnapshot {
                    id,
                    kind,
                    timebase,
                    origin,
                    source_timestamp: None,
                    normalized_timestamp: None,
                    source_packets: 0,
                    source_bytes: 0,
                    normalized_samples: 0,
                    normalized_seconds: 0.0,
                    normalized_end: None,
                },
            );
        }
    }
    pub fn input(&self, id: TrackId, bytes: usize) {
        if let Some(track) = self.0.lock().get_mut(&id) {
            track.source_timestamp = Some(timestamp());
            track.source_packets += 1;
            track.source_bytes += bytes as u64;
        }
    }
    // Exported seconds use the Prometheus double representation.
    #[allow(clippy::cast_precision_loss)]
    pub fn normalized(&self, id: TrackId, pts: i64, duration: u64) {
        if let Some(track) = self.0.lock().get_mut(&id) {
            track.normalized_timestamp = Some(timestamp());
            track.normalized_samples += 1;
            track.normalized_seconds += track.timebase.ticks_to_duration(duration).as_secs_f64();
            let end = (i128::from(pts) - i128::from(track.origin) + i128::from(duration)) as f64
                * f64::from(track.timebase.num().get())
                / f64::from(track.timebase.den().get());
            track.normalized_end = Some(
                track
                    .normalized_end
                    .map_or(end, |previous| previous.max(end)),
            );
        }
    }
    pub fn snapshot(&self) -> Vec<TrackSnapshot> {
        self.0.lock().values().cloned().collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn sibling_activity_does_not_hide_an_unstarted_track() {
        let meters = TrackMeters::default();
        meters.register(
            [
                (TrackId(0), MediaKind::Video, Timebase::hz90k(), 90_000),
                (
                    TrackId(1),
                    MediaKind::Audio,
                    Timebase::new(nz::u32!(1), nz::u32!(48_000)),
                    0,
                ),
            ]
            .into_iter(),
        );
        meters.input(TrackId(0), 100);
        meters.normalized(TrackId(0), 90_000, 90_000);
        meters.normalized(TrackId(99), 0, 1);
        let snapshot = meters.snapshot();
        assert_eq!(
            snapshot.len(),
            2,
            "unknown track IDs cannot grow cardinality"
        );
        assert!((snapshot[0].normalized_seconds - 1.0).abs() < f64::EPSILON);
        assert_eq!(snapshot[0].normalized_end, Some(1.0));
        assert!(snapshot[0].source_timestamp.is_some());
        assert_eq!(snapshot[1].source_timestamp, None);
        assert_eq!(snapshot[1].normalized_timestamp, None);
    }
}
