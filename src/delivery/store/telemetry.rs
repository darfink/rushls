//! Publication timing follows successful media commits, independently of pacing.
//!
//! This state is small and separate from retained payloads. The store serializes
//! mutations with publication ownership; scrapes only take this short mutex.

use crate::{
    domain::{MediaKind, RenditionId, Timebase, TrackId},
    mux::{PackagedMedia, PackagedRendition},
    observe::DurationHistogram,
};
use parking_lot::Mutex;
use std::{
    collections::{BTreeMap, VecDeque},
    sync::Arc,
    time::{Duration, SystemTime},
};
use tokio::time::Instant;

const HISTORY_LIMIT: usize = 256;

#[derive(Clone, Debug, Default)]
pub struct PublicationTotals(pub Arc<Mutex<PublicationTotalSnapshot>>);

#[derive(Clone, Debug, Default)]
pub struct PublicationTotalSnapshot {
    pub gaps: u64,
    pub deadline_misses: u64,
    pub timeline_breaks: u64,
    pub incomplete_comparisons: u64,
    pub intervals: DurationHistogram,
    pub spreads: DurationHistogram,
}

#[derive(Clone, Copy, Debug)]
struct Interval {
    start: i128,
    end: i128,
    at: Instant,
}

#[derive(Clone, Debug)]
pub struct RenditionTiming {
    pub id: RenditionId,
    pub kind: MediaKind,
    pub sources: Arc<[TrackId]>,
    pub interval: Duration,
    pub target: Duration,
    pub part_target: Option<Duration>,
    pub startup_budget: Duration,
    pub tolerance: Duration,
    pub media_seconds: f64,
    pub deadline_misses: u64,
    pub timeline_breaks: u64,
    pub last_timestamp: Option<f64>,
    timebase: Timebase,
    started_at: Instant,
    last: Option<Instant>,
    /// Once a timestamp gap occurs, this publication's contiguous edge stops.
    /// Counting a later timestamp as catch-up would hide missing media.
    edge: Option<i128>,
    broken: bool,
    observed_end: Option<i128>,
    deadline_counted: bool,
    history: Arc<VecDeque<Interval>>,
}

impl RenditionTiming {
    fn new(
        id: RenditionId,
        descriptor: &PackagedRendition,
        now: Instant,
        target: Duration,
    ) -> Self {
        let config = descriptor.config;
        let part_target = config
            .chunk_target
            .map(|t| config.timebase.ticks_to_duration(t.get()));
        let interval = part_target.unwrap_or_else(|| {
            config
                .timebase
                .ticks_to_duration(config.segment_target.get())
        });
        // One planned interval of publication jitter, explicit in the export.
        // Startup can require a full segment/keyframe before the first part.
        let tolerance = interval;
        Self {
            id,
            kind: descriptor.media.kind(),
            sources: descriptor.source_tracks.clone(),
            interval,
            target,
            part_target,
            startup_budget: config
                .timebase
                .ticks_to_duration(config.maximum_segment_duration.get())
                .saturating_add(tolerance),
            tolerance,
            media_seconds: 0.0,
            deadline_misses: 0,
            timeline_breaks: 0,
            last_timestamp: None,
            timebase: config.timebase,
            started_at: now,
            last: None,
            edge: None,
            broken: false,
            observed_end: None,
            deadline_counted: false,
            history: Arc::new(VecDeque::new()),
        }
    }

    fn continuous(&self) -> bool {
        self.kind != MediaKind::Subtitle
    }

    fn overdue(&self, now: Instant) -> Duration {
        let (origin, budget) = self
            .last
            .map_or((self.started_at, self.startup_budget), |last| {
                (last, self.interval.saturating_add(self.tolerance))
            });
        now.saturating_duration_since(origin).saturating_sub(budget)
    }

    fn tick(&mut self, now: Instant, totals: &PublicationTotals) {
        if self.continuous() && !self.deadline_counted && !self.overdue(now).is_zero() {
            self.deadline_counted = true;
            self.deadline_misses += 1;
            totals.0.lock().deadline_misses += 1;
        }
    }

    fn arrival(&self, boundary: i128) -> Option<Instant> {
        self.history
            .iter()
            .find(|i| i.start < boundary && boundary <= i.end)
            .map(|i| i.at)
    }
}

#[derive(Clone, Debug)]
struct Comparison {
    boundary: i128,
    first: Instant,
    arrivals: Vec<Option<Instant>>,
}

#[derive(Clone, Debug)]
pub struct GroupTiming {
    pub name: Arc<str>,
    pub members: Vec<RenditionId>,
    pub spread: Option<Duration>,
    pub spread_timestamp: Option<f64>,
    pub incomplete: u64,
    pending: VecDeque<Comparison>,
    last_boundary: Option<i128>,
}

#[derive(Clone, Debug, Default)]
pub struct PublicationTelemetry {
    pub publication: u64,
    pub active: bool,
    pub renditions: BTreeMap<RenditionId, RenditionTiming>,
    pub groups: Vec<GroupTiming>,
    /// Shared baseline preserves initial offsets between siblings.
    baseline: Option<(Instant, i128)>,
    pub totals: PublicationTotals,
}

#[derive(Clone, Debug)]
pub struct RenditionTimingSnapshot {
    pub timing: RenditionTiming,
    pub expected: bool,
    pub startup_elapsed: Duration,
    pub overdue: Duration,
    pub lag: Option<f64>,
    pub media_end: Option<f64>,
}

#[derive(Clone, Debug)]
pub struct GroupTimingSnapshot {
    pub name: Arc<str>,
    pub members: Vec<RenditionId>,
    pub skew: Option<f64>,
    pub spread: Option<Duration>,
    pub spread_timestamp: Option<f64>,
    pub pending: usize,
    pub pending_age: Duration,
    pub incomplete: u64,
}

#[derive(Clone, Debug)]
pub struct PublicationSnapshot {
    pub active: bool,
    pub renditions: Vec<RenditionTimingSnapshot>,
    pub groups: Vec<GroupTimingSnapshot>,
}

pub fn unix_now() -> f64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs_f64()
}

/// Prometheus samples are doubles; keep comparisons exact until this boundary.
#[allow(clippy::cast_precision_loss)]
fn seconds(nanos: i128) -> f64 {
    nanos as f64 / 1e9
}

fn nanos(ticks: i128, timebase: Timebase) -> i128 {
    ticks * i128::from(timebase.num().get()) * 1_000_000_000 / i128::from(timebase.den().get())
}

impl PublicationTelemetry {
    pub fn attach(
        &mut self,
        publication: u64,
        descriptors: impl Iterator<Item = (RenditionId, PackagedRendition, Duration)>,
        groups: Vec<(Arc<str>, Vec<RenditionId>)>,
    ) {
        let now = Instant::now();
        self.stop(self.publication);
        let previous = std::mem::take(&mut self.renditions);
        self.publication = publication;
        self.active = true;
        self.baseline = None;
        for (id, descriptor, target) in descriptors {
            let mut timing = RenditionTiming::new(id, &descriptor, now, target);
            if let Some(old) = previous.get(&id) {
                timing.media_seconds = old.media_seconds;
                timing.deadline_misses = old.deadline_misses;
                timing.timeline_breaks = old.timeline_breaks;
            }
            self.renditions.insert(id, timing);
        }
        self.groups = groups
            .into_iter()
            .filter_map(|(name, members)| {
                let members: Vec<_> = members
                    .into_iter()
                    .filter(|id| {
                        self.renditions
                            .get(id)
                            .is_some_and(RenditionTiming::continuous)
                    })
                    .collect();
                (members.len() > 1).then_some(GroupTiming {
                    name,
                    members,
                    spread: None,
                    spread_timestamp: None,
                    incomplete: 0,
                    pending: VecDeque::new(),
                    last_boundary: None,
                })
            })
            .collect();
    }

    pub fn tick(&mut self) {
        if self.active {
            let now = Instant::now();
            for rendition in self.renditions.values_mut() {
                rendition.tick(now, &self.totals);
            }
        }
    }

    pub fn stop(&mut self, publication: u64) {
        if self.publication != publication || !self.active {
            return;
        }
        self.tick();
        self.active = false;
        for group in &mut self.groups {
            let count = group.pending.len() as u64;
            group.incomplete += count;
            self.totals.0.lock().incomplete_comparisons += count;
            group.pending.clear();
        }
    }

    /// Capture timing before the media value moves, but observe it only after commit.
    pub fn interval(media: &PackagedMedia) -> Option<(i64, u64)> {
        match media {
            PackagedMedia::Chunk(c) => Some((c.media_start, c.duration)),
            PackagedMedia::Segment(s) => Some((s.media_start, s.duration)),
            PackagedMedia::Initialization(_) | PackagedMedia::SegmentCompleted(_) => None,
        }
    }

    pub fn committed(&mut self, publication: u64, id: RenditionId, interval: Option<(i64, u64)>) {
        if self.publication != publication {
            return;
        }
        let Some((start, duration)) = interval else {
            return;
        };
        let Some(rendition) = self.renditions.get_mut(&id) else {
            return;
        };
        if duration == 0 {
            return;
        }
        let now = Instant::now();
        let timestamp = unix_now();
        if self.active {
            rendition.tick(now, &self.totals);
        }
        let end = nanos(i128::from(start) + i128::from(duration), rendition.timebase);
        let start = nanos(i128::from(start), rendition.timebase);
        // Nanosecond conversion can round adjacent intervals by one nanosecond.
        if rendition.edge.is_some_and(|edge| (start - edge).abs() > 1) && !rendition.broken {
            rendition.broken = true;
            rendition.timeline_breaks += 1;
            self.totals.0.lock().timeline_breaks += 1;
        }
        let new_start = rendition
            .observed_end
            .map_or(start, |previous| previous.max(start));
        rendition.media_seconds += seconds((end - new_start).max(0));
        rendition.observed_end = Some(
            rendition
                .observed_end
                .map_or(end, |previous| previous.max(end)),
        );
        if self.active {
            if let Some(last) = rendition.last
                && rendition.continuous()
            {
                self.totals
                    .0
                    .lock()
                    .intervals
                    .observe(now.saturating_duration_since(last));
            }
            if !rendition.broken {
                if rendition.continuous() {
                    self.baseline.get_or_insert((now, start));
                }
                rendition.edge = Some(end);
            }
            rendition.last = Some(now);
            rendition.last_timestamp = Some(timestamp);
            rendition.deadline_counted = false;
            if !rendition.broken {
                let history = Arc::make_mut(&mut rendition.history);
                history.push_back(Interval {
                    start,
                    end,
                    at: now,
                });
                if history.len() > HISTORY_LIMIT {
                    history.pop_front();
                }
                self.compare(id, end, now, timestamp);
            }
        }
    }

    fn compare(&mut self, id: RenditionId, boundary: i128, now: Instant, timestamp: f64) {
        for group in &mut self.groups {
            if !group.members.contains(&id) {
                continue;
            }
            // Each new maximum boundary is sampled once. Slower arrivals resolve
            // existing comparisons rather than adding duplicate history.
            if group.last_boundary.is_none_or(|last| boundary > last) {
                group.last_boundary = Some(boundary);
                group.pending.push_back(Comparison {
                    boundary,
                    first: now,
                    arrivals: vec![None; group.members.len()],
                });
            }
            for comparison in &mut group.pending {
                for (index, member) in group.members.iter().enumerate() {
                    if comparison.arrivals[index].is_none() {
                        comparison.arrivals[index] =
                            self.renditions[member].arrival(comparison.boundary);
                    }
                }
            }
            group.pending.retain(|comparison| {
                let Some(arrivals) = comparison
                    .arrivals
                    .iter()
                    .copied()
                    .collect::<Option<Vec<_>>>()
                else {
                    return true;
                };
                let first = *arrivals.iter().min().expect("comparison has members");
                let last = *arrivals.iter().max().expect("comparison has members");
                let spread = last.saturating_duration_since(first);
                group.spread = Some(spread);
                group.spread_timestamp = Some(timestamp);
                self.totals.0.lock().spreads.observe(spread);
                false
            });
            while group.pending.len() > HISTORY_LIMIT {
                group.pending.pop_front();
                group.incomplete += 1;
                self.totals.0.lock().incomplete_comparisons += 1;
            }
        }
    }

    pub fn snapshot(&mut self) -> PublicationSnapshot {
        self.tick();
        let now = Instant::now();
        let renditions = self
            .renditions
            .values()
            .map(|r| {
                let expected = self.active && r.continuous();
                RenditionTimingSnapshot {
                    timing: r.clone(),
                    expected,
                    startup_elapsed: if expected && r.last.is_none() {
                        now.saturating_duration_since(r.started_at)
                    } else {
                        Duration::ZERO
                    },
                    overdue: if expected {
                        r.overdue(now)
                    } else {
                        Duration::ZERO
                    },
                    lag: if expected {
                        self.baseline.zip(r.edge).map(|((at, origin), edge)| {
                            now.saturating_duration_since(at).as_secs_f64() - seconds(edge - origin)
                        })
                    } else {
                        None
                    },
                    media_end: r.edge.map(seconds),
                }
            })
            .collect();
        let groups = self
            .groups
            .iter()
            .map(|g| {
                let edges: Option<Vec<_>> = g
                    .members
                    .iter()
                    .map(|id| self.renditions[id].edge)
                    .collect();
                let skew = edges.filter(|_| self.active).map(|edges| {
                    seconds(
                        *edges.iter().max().expect("members")
                            - *edges.iter().min().expect("members"),
                    )
                });
                GroupTimingSnapshot {
                    name: g.name.clone(),
                    members: g.members.clone(),
                    skew,
                    spread: g.spread,
                    spread_timestamp: g.spread_timestamp,
                    pending: g.pending.len(),
                    pending_age: g
                        .pending
                        .front()
                        .map_or(Duration::ZERO, |p| now.saturating_duration_since(p.first)),
                    incomplete: g.incomplete,
                }
            })
            .collect();
        PublicationSnapshot {
            active: self.active,
            renditions,
            groups,
        }
    }
}

#[cfg(test)]
mod tests;
