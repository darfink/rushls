//! Publication-wide segment decisions. Only undecided boundary media is held.
use super::{FinishReason, MuxError, Muxer, PackagedMedia, TrackPackager};
use crate::{
    domain::{Appender, MediaInstant, MediaKind, TickTimestamp},
    media::{NormalizedSample, PresentationPlan},
    segment::{PrerollLimits, SegmentationPlan, TrackSegmentationPlan},
};
use std::{cmp::Ordering, collections::VecDeque, time::Duration};

struct Track {
    plan: TrackSegmentationPlan,
    kind: MediaKind,
    writer: Box<dyn TrackPackager>,
    pending: VecDeque<NormalizedSample>,
    start: TickTimestamp,
}
impl Track {
    fn instant(&self, pts: TickTimestamp) -> MediaInstant {
        MediaInstant::new(self.plan.timebase, pts, self.plan.presentation_origin_pts)
    }
}

type Cuts = Vec<Option<(usize, TickTimestamp)>>;

pub struct Coordinator {
    tracks: Vec<Track>,
    authority: usize,
    nominal: TickTimestamp,
    boundary_index: u64,
    early: u64,
    late: u64,
    boundary_budget: Duration,
    limits: PrerollLimits,
    samples: usize,
    bytes: usize,
    interval: Duration,
    finished: bool,
    events: crate::observe::EventSink,
}
impl Coordinator {
    pub fn new(
        writers: Vec<Box<dyn TrackPackager>>,
        presentation: &PresentationPlan,
        plan: &SegmentationPlan,
        events: crate::observe::EventSink,
    ) -> Result<Self, MuxError> {
        let mut tracks = Vec::new();
        for writer in writers {
            let id = writer.track_id();
            let track_plan = *plan
                .get(id)
                .ok_or_else(|| MuxError::InvalidPlan("missing coordinator track".into()))?;
            let kind = presentation
                .catalog()
                .get(id)
                .ok_or_else(|| MuxError::InvalidPlan("missing source track".into()))?
                .kind();
            tracks.push(Track {
                plan: track_plan,
                kind,
                writer,
                pending: VecDeque::new(),
                start: track_plan.segmentation_origin_pts,
            });
        }
        let authority = tracks
            .iter()
            .position(|track| track.kind == MediaKind::Video)
            .or_else(|| {
                tracks
                    .iter()
                    .position(|track| track.kind == MediaKind::Audio)
            })
            .ok_or_else(|| MuxError::InvalidPlan("coordinator requires audio or video".into()))?;
        let source = &tracks[authority].plan;
        Ok(Self {
            nominal: source.first_segment_boundary_pts,
            boundary_index: 1,
            // The configured allowances are operator slack on top of the
            // quantization the boundary grid already has.
            //
            // Only video needs it on the early side. Where every access unit is
            // a random-access point, the planner rounds up to the first unit at
            // or after the planned instant, so the achieved boundary is late by
            // construction and never early; admitting an early unit there would
            // pull every boundary back onto the preceding unit and ratchet the
            // segment down to one access unit. Video random access is sparse
            // and its real period need not be an integer number of ticks, so
            // its achieved boundary lands on either side of the planned one.
            early: source
                .timebase
                .duration_to_ticks_floor(plan.early_boundary)
                .saturating_add(if tracks[authority].kind == MediaKind::Video {
                    source.boundary_tolerance
                } else {
                    0
                }),
            late: source
                .timebase
                .duration_to_ticks_floor(plan.late_boundary)
                .saturating_add(source.boundary_tolerance),
            boundary_budget: plan.late_boundary.saturating_add(plan.early_boundary),
            tracks,
            authority,
            limits: plan.limits,
            samples: 0,
            bytes: 0,
            interval: plan.shortest_part_duration().saturating_mul(2),
            finished: false,
            events,
        })
    }

    fn compare(a: MediaInstant, b: MediaInstant) -> Result<Ordering, MuxError> {
        a.compare(b)
            .ok_or_else(|| MuxError::Mux("boundary comparison overflowed".into()))
    }

    fn window(&self) -> Result<(MediaInstant, MediaInstant), MuxError> {
        let source = &self.tracks[self.authority];
        let early = self
            .nominal
            .checked_sub_unsigned(self.early)
            .ok_or_else(|| MuxError::Mux("early boundary overflowed".into()))?;
        let late = self
            .nominal
            .checked_add_unsigned(self.late)
            .ok_or_else(|| MuxError::Mux("late boundary overflowed".into()))?;
        Ok((source.instant(early), source.instant(late)))
    }

    fn release(
        &mut self,
        index: usize,
        count: usize,
        out: &mut dyn Appender<PackagedMedia>,
    ) -> Result<(), MuxError> {
        for _ in 0..count {
            let sample = self.tracks[index]
                .pending
                .pop_front()
                .expect("count is within queue");
            self.samples -= 1;
            self.bytes -= sample.retained_bytes();
            let now = self.tracks[index].instant(sample.pts());
            self.tracks[index].writer.push(sample, out)?;
            for track in &mut self.tracks {
                if track.kind == MediaKind::Subtitle {
                    track.writer.tick(now, out)?;
                }
            }
        }
        Ok(())
    }

    fn select(&self, early: MediaInstant, late: MediaInstant) -> Result<Option<Cuts>, MuxError> {
        for candidate in &self.tracks[self.authority].pending {
            if !candidate.random_access() {
                continue;
            }
            let instant = self.tracks[self.authority].instant(candidate.pts());
            if Self::compare(instant, early)? == Ordering::Less
                || Self::compare(instant, late)? == Ordering::Greater
            {
                continue;
            }
            let mut cuts = Vec::new();
            for track in &self.tracks {
                if track.kind == MediaKind::Subtitle {
                    cuts.push(None);
                    continue;
                }
                let mut found = None;
                for (index, sample) in track.pending.iter().enumerate() {
                    let order = Self::compare(track.instant(sample.pts()), instant)?;
                    if (track.kind == MediaKind::Video
                        && sample.random_access()
                        && order == Ordering::Equal)
                        || (track.kind == MediaKind::Audio && order != Ordering::Less)
                    {
                        found = Some((index, sample.pts()));
                        break;
                    }
                }
                cuts.push(found);
            }
            if self
                .tracks
                .iter()
                .zip(&cuts)
                .all(|(track, cut)| track.kind == MediaKind::Subtitle || cut.is_some())
            {
                return Ok(Some(cuts));
            }
        }
        Ok(None)
    }

    fn window_failure(&self, track: &Track, reason: &'static str) -> MuxError {
        let authority = &self.tracks[self.authority];
        let progress = authority
            .pending
            .iter()
            .map(|sample| match sample {
                NormalizedSample::Video(video) => video.dts,
                _ => sample.pts(),
            })
            .max()
            .unwrap_or(self.nominal);
        MuxError::BoundaryWindow {
            track: track.plan.track_id,
            reason,
            progress,
            earliest: self.nominal.saturating_sub_unsigned(self.early),
            latest: self.nominal.saturating_add_unsigned(self.late),
            timebase: authority.plan.timebase,
        }
    }

    fn require_possible(&self, late: MediaInstant) -> Result<(), MuxError> {
        let videos: Vec<_> = self
            .tracks
            .iter()
            .filter(|track| track.kind == MediaKind::Video)
            .collect();
        let common_video = self.tracks[self.authority].pending.iter().any(|candidate| {
            let instant = self.tracks[self.authority].instant(candidate.pts());
            candidate.random_access()
                && Self::compare(instant, late).is_ok_and(|order| order != Ordering::Greater)
                && videos.iter().all(|track| {
                    track.pending.iter().any(|sample| {
                        sample.random_access()
                            && Self::compare(track.instant(sample.pts()), instant)
                                == Ok(Ordering::Equal)
                    })
                })
        });
        if !videos.is_empty()
            && !common_video
            && videos.iter().all(|track| {
                track.pending.iter().any(|sample| match sample {
                    NormalizedSample::Video(video) => {
                        Self::compare(track.instant(video.dts), late) == Ok(Ordering::Greater)
                    }
                    _ => false,
                })
            })
        {
            return Err(self.window_failure(videos[0], "misaligned random-access boundaries"));
        }

        for track in &self.tracks {
            if track.kind != MediaKind::Video {
                continue;
            }
            // DTS is a conservative progress watermark for reordered
            // pictures; PTS alone cannot prove a missing earlier RAP.
            if track.pending.iter().any(|sample| match sample {
                NormalizedSample::Video(video) => {
                    Self::compare(track.instant(video.dts), late) == Ok(Ordering::Greater)
                }
                _ => false,
            }) && !track.pending.iter().any(|sample| {
                sample.random_access()
                    && Self::compare(track.instant(sample.pts()), late)
                        .is_ok_and(|order| order != Ordering::Greater)
            }) {
                return Err(self.window_failure(track, "missing common random-access boundary"));
            }
        }
        Ok(())
    }

    fn commit(
        &mut self,
        cuts: Cuts,
        out: &mut dyn Appender<PackagedMedia>,
    ) -> Result<(), MuxError> {
        for (index, cut) in cuts.into_iter().enumerate() {
            if let Some((count, pts)) = cut {
                let track = &self.tracks[index];
                let actual = pts
                    .checked_sub(track.start)
                    .and_then(|ticks| u64::try_from(ticks).ok())
                    .ok_or_else(|| MuxError::Mux("invalid coordinated segment span".into()))?;
                let maximum = track.plan.maximum_segment_ticks(self.boundary_budget);
                if actual > maximum {
                    return Err(MuxError::Boundary {
                        track: track.plan.track_id,
                        reason: "segment ceiling exhausted",
                        observed: actual,
                        maximum,
                    });
                }
                self.release(index, count, out)?;
                self.tracks[index].writer.cut(pts, out)?;
                let track = &mut self.tracks[index];
                let actual = pts
                    .checked_sub(track.start)
                    .and_then(|ticks| u64::try_from(ticks).ok())
                    .unwrap_or(0);
                if actual > track.plan.segment_duration.get() && track.kind == MediaKind::Video {
                    self.events
                        .emit(crate::observe::SessionEvent::SegmentationExtended {
                            track: track.plan.track_id,
                            planned: track
                                .plan
                                .timebase
                                .ticks_to_duration(track.plan.segment_duration.get()),
                            actual: track.plan.timebase.ticks_to_duration(actual),
                        });
                }
                track.start = pts;
            }
        }
        Ok(())
    }

    fn drive(&mut self, out: &mut dyn Appender<PackagedMedia>) -> Result<(), MuxError> {
        loop {
            let (early, late) = self.window()?;
            // Decode order is preserved. A future reference picture holds the
            // following B-pictures until the selected RAP resolves membership.
            for index in 0..self.tracks.len() {
                if self.tracks[index].kind == MediaKind::Subtitle {
                    continue;
                }
                let count = self.tracks[index]
                    .pending
                    .iter()
                    .take_while(|sample| {
                        Self::compare(self.tracks[index].instant(sample.pts()), early)
                            == Ok(Ordering::Less)
                    })
                    .count();
                self.release(index, count, out)?;
            }
            let selection = self.select(early, late)?;
            let Some(cuts) = selection else {
                self.require_possible(late)?;
                return Ok(());
            };
            self.commit(cuts, out)?;
            self.boundary_index = self
                .boundary_index
                .checked_add(1)
                .ok_or_else(|| MuxError::Mux("boundary index overflowed".into()))?;
            let plan = self.tracks[self.authority].plan;
            let offset = if let Some((numerator, denominator)) = plan.segment_period {
                u64::try_from(
                    (u128::from(self.boundary_index) * u128::from(numerator)
                        + u128::from(denominator) / 2)
                        / u128::from(denominator),
                )
                .map_err(|_| MuxError::Mux("nominal boundary overflowed".into()))?
            } else {
                self.boundary_index
                    .saturating_sub(1)
                    .checked_mul(plan.segment_duration.get())
                    .ok_or_else(|| MuxError::Mux("nominal boundary overflowed".into()))?
            };
            self.nominal = (if plan.segment_period.is_some() {
                plan.segmentation_origin_pts
            } else {
                plan.first_segment_boundary_pts
            })
            .checked_add_unsigned(offset)
            .ok_or_else(|| MuxError::Mux("nominal boundary overflowed".into()))?;
        }
    }
}
impl Muxer for Coordinator {
    fn expected_publication_interval(&self) -> Duration {
        self.interval
    }
    fn push(
        &mut self,
        sample: NormalizedSample,
        out: &mut dyn Appender<PackagedMedia>,
    ) -> Result<(), MuxError> {
        if self.finished {
            return Err(MuxError::Mux("coordinator already finished".into()));
        }
        let index = self
            .tracks
            .iter()
            .position(|track| track.plan.track_id == sample.track_id())
            .ok_or_else(|| MuxError::Mux("unknown coordinator track".into()))?;
        if self.tracks[index].kind == MediaKind::Subtitle {
            return self.tracks[index].writer.push(sample, out);
        }
        let bytes = self.bytes.saturating_add(sample.retained_bytes());
        let samples = self.samples.saturating_add(1);
        let (writer_bytes, writer_samples) = self
            .tracks
            .iter()
            .map(|track| track.writer.buffered())
            .fold((0_usize, 0_usize), |(bytes, samples), (b, s)| {
                (bytes.saturating_add(b), samples.saturating_add(s))
            });
        if bytes.saturating_add(writer_bytes) > self.limits.maximum_buffered_bytes
            || samples.saturating_add(writer_samples) > self.limits.maximum_buffered_samples
        {
            return Err(MuxError::CoordinatorLimit {
                bytes: bytes.saturating_add(writer_bytes),
                samples: samples.saturating_add(writer_samples),
            });
        }
        self.bytes = bytes;
        self.samples = samples;
        self.tracks[index].pending.push_back(sample);
        self.drive(out)
    }
    fn finish(
        &mut self,
        reason: FinishReason,
        out: &mut dyn Appender<PackagedMedia>,
    ) -> Result<(), MuxError> {
        if self.finished {
            return Ok(());
        }
        self.finished = true;
        let mut failure = self.drive(out).err();
        for index in 0..self.tracks.len() {
            if failure.is_none() && !matches!(reason, FinishReason::Superseded) {
                let count = self.tracks[index].pending.len();
                failure = self.release(index, count, out).err();
            }
            // Release every writer even when one rendition fails. Once the
            // contract is broken, remaining unresolved media cannot be drained.
            let finish_reason = if failure.is_some() {
                FinishReason::Superseded
            } else {
                reason
            };
            let result = self.tracks[index].writer.finish(finish_reason, out);
            if failure.is_none() {
                failure = result.err();
            }
            self.tracks[index].pending.clear();
        }
        self.samples = 0;
        self.bytes = 0;
        failure.map_or(Ok(()), Err)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        admission::StreamPolicy,
        domain::{
            Codec, Payload, Timebase, TrackId,
            fixtures::{TrackBuilder, catalog},
        },
        media::{VideoSample, validate},
        segment::fixtures::PlanBuilder,
    };
    use parking_lot::Mutex;
    use std::sync::Arc;

    #[derive(Clone, Debug, PartialEq)]
    enum Action {
        Sample(TrackId, i64),
        Cut(TrackId, i64),
        Finished(TrackId),
    }
    struct Writer {
        id: TrackId,
        actions: Arc<Mutex<Vec<Action>>>,
    }
    impl TrackPackager for Writer {
        fn track_id(&self) -> TrackId {
            self.id
        }
        fn push(
            &mut self,
            sample: NormalizedSample,
            _: &mut dyn Appender<PackagedMedia>,
        ) -> Result<(), MuxError> {
            self.actions
                .lock()
                .push(Action::Sample(self.id, sample.pts()));
            Ok(())
        }
        fn cut(&mut self, pts: i64, _: &mut dyn Appender<PackagedMedia>) -> Result<(), MuxError> {
            self.actions.lock().push(Action::Cut(self.id, pts));
            Ok(())
        }
        fn finish(
            &mut self,
            _: FinishReason,
            _: &mut dyn Appender<PackagedMedia>,
        ) -> Result<(), MuxError> {
            self.actions.lock().push(Action::Finished(self.id));
            Ok(())
        }
    }
    type Actions = Arc<Mutex<Vec<Action>>>;
    fn coordinator(
        early: u64,
        late: u64,
    ) -> Result<(Coordinator, Actions), Box<dyn std::error::Error>> {
        coordinator_with_tolerance(early, late, 0)
    }
    fn coordinator_with_tolerance(
        early: u64,
        late: u64,
        tolerance: u64,
    ) -> Result<(Coordinator, Actions), Box<dyn std::error::Error>> {
        let timebase = Timebase::new(nz::u32!(1), nz::u32!(1_000));
        let input = validate(
            &catalog(
                (0..3)
                    .map(|id| {
                        TrackBuilder::new(id, MediaKind::Video)
                            .timebase(timebase)
                            .build()
                    })
                    .collect(),
            ),
            &StreamPolicy::permissive(),
        )?;
        let mut plan = SegmentationPlan::new(
            &input,
            (0..3)
                .map(|id| {
                    PlanBuilder::new(id, timebase, nz::u64!(2_000))
                        .boundary_tolerance(tolerance)
                        .build()
                })
                .collect(),
        )?;
        plan.early_boundary = Duration::from_millis(early);
        plan.late_boundary = Duration::from_millis(late);
        let actions = Arc::new(Mutex::new(Vec::new()));
        let writers = (0..3)
            .map(|id| {
                Box::new(Writer {
                    id: TrackId(id),
                    actions: actions.clone(),
                }) as Box<dyn TrackPackager>
            })
            .collect();
        let events =
            crate::observe::Events::default().scoped(crate::domain::SessionId(nz::u64!(1)));
        Ok((Coordinator::new(writers, &input, &plan, events)?, actions))
    }
    fn sample(id: u32, pts: i64, key: bool) -> NormalizedSample {
        NormalizedSample::Video(VideoSample {
            track_id: TrackId(id),
            codec: Codec::H264,
            pts,
            dts: pts,
            duration: 40,
            random_access: key,
            payload: Payload::default(),
        })
    }
    #[test]
    fn the_ladder_waits_for_the_last_matching_rap() -> Result<(), Box<dyn std::error::Error>> {
        let (mut mux, actions) = coordinator(0, 0)?;
        let mut output = Vec::new();
        for id in 0..3 {
            mux.push(sample(id, 0, true), &mut output)?;
        }
        for id in 0..2 {
            mux.push(sample(id, 2_000, true), &mut output)?;
        }
        assert!(
            !actions
                .lock()
                .iter()
                .any(|action| matches!(action, Action::Cut(..)))
        );
        mux.push(sample(2, 2_000, true), &mut output)?;
        assert_eq!(
            actions
                .lock()
                .iter()
                .filter(|action| matches!(action, Action::Cut(..)))
                .count(),
            3
        );
        Ok(())
    }
    #[test]
    fn early_then_late_cuts_preserve_the_nominal_grid() -> Result<(), Box<dyn std::error::Error>> {
        let (mut mux, actions) = coordinator(200, 200)?;
        let mut output = Vec::new();
        // Early then late spans the full 2.4-second advertised budget.
        for pts in [0, 1_800, 4_200, 6_000] {
            for id in 0..3 {
                mux.push(sample(id, pts, true), &mut output)?;
            }
        }
        let cuts: Vec<_> = actions
            .lock()
            .iter()
            .filter_map(|action| match action {
                Action::Cut(TrackId(0), pts) => Some(*pts),
                _ => None,
            })
            .collect();
        assert_eq!(cuts, [1_800, 4_200, 6_000]);
        assert_eq!(mux.nominal, 8_000);
        Ok(())
    }

    #[test]
    fn a_non_integer_keyframe_period_does_not_drift_out_of_the_window()
    -> Result<(), Box<dyn std::error::Error>> {
        let (mut mux, actions) = coordinator_with_tolerance(0, 0, 1)?;
        // A half-tick period alternates between 2000 and 2001 ticks.
        for track in &mut mux.tracks {
            track.plan.segment_period = Some((4_001, 2));
        }
        let mut output = Vec::new();
        for segment in 0..200_i64 {
            for id in 0..3 {
                mux.push(sample(id, (segment * 4_001 + 1) / 2, true), &mut output)?;
            }
        }
        let cuts = actions
            .lock()
            .iter()
            .filter(|action| matches!(action, Action::Cut(TrackId(0), _)))
            .count();
        assert_eq!(cuts, 199);
        Ok(())
    }
    #[test]
    fn a_stopped_sibling_cannot_grow_memory_without_bound() -> Result<(), Box<dyn std::error::Error>>
    {
        let (mut mux, _) = coordinator(0, 0)?;
        mux.limits.maximum_buffered_samples = 3;
        let mut output = Vec::new();
        for pts in [2_000, 2_040, 2_080] {
            mux.push(sample(0, pts, pts == 2_000), &mut output)?;
        }
        assert!(matches!(
            mux.push(sample(0, 2_120, false), &mut output),
            Err(MuxError::CoordinatorLimit { .. })
        ));
        Ok(())
    }
    #[test]
    fn a_track_without_a_rap_fails_after_its_window() -> Result<(), Box<dyn std::error::Error>> {
        let (mut mux, _) = coordinator(0, 100)?;
        let mut output = Vec::new();
        mux.push(sample(0, 2_000, false), &mut output)?;
        assert!(matches!(
            mux.push(sample(0, 2_101, false), &mut output),
            Err(MuxError::BoundaryWindow { .. })
        ));
        Ok(())
    }
    #[test]
    fn matching_video_instants_do_not_require_equal_tick_values()
    -> Result<(), Box<dyn std::error::Error>> {
        let (mut mux, actions) = coordinator(0, 0)?;
        mux.tracks[1].plan.timebase = Timebase::hz90k();
        mux.tracks[1].plan.segment_duration = nz::u64!(180_000);
        mux.tracks[1].plan.first_segment_boundary_pts = 180_000;
        let mut out = Vec::new();
        for id in 0..3 {
            mux.push(sample(id, 0, true), &mut out)?;
        }
        mux.push(sample(0, 2_000, true), &mut out)?;
        mux.push(sample(1, 180_000, true), &mut out)?;
        mux.push(sample(2, 2_000, true), &mut out)?;
        assert!(actions.lock().contains(&Action::Cut(TrackId(1), 180_000)));
        Ok(())
    }

    #[test]
    fn coordinator_checks_bytes_even_when_the_sample_count_fits()
    -> Result<(), Box<dyn std::error::Error>> {
        let (mut mux, _) = coordinator(0, 0)?;
        mux.limits.maximum_buffered_bytes = 1;
        assert!(matches!(
            mux.push(sample(0, 2_000, true), &mut Vec::new()),
            Err(MuxError::CoordinatorLimit { .. })
        ));
        Ok(())
    }

    #[test]
    fn a_boundary_failure_still_finishes_every_writer() -> Result<(), Box<dyn std::error::Error>> {
        let (mut mux, actions) = coordinator(0, 0)?;
        let mut out = Vec::new();
        assert!(mux.push(sample(0, 2_001, false), &mut out).is_err());
        assert!(mux.finish(FinishReason::Superseded, &mut out).is_err());
        for id in 0..3 {
            assert!(actions.lock().contains(&Action::Finished(TrackId(id))));
        }
        assert_eq!((mux.samples, mux.bytes), (0, 0));
        assert!(mux.tracks.iter().all(|track| track.pending.is_empty()));
        Ok(())
    }
    /// A writer that retains its input models the repair window without
    /// depending on codec bytes or the container serializer.
    struct HoldingWriter {
        id: crate::domain::TrackId,
        samples: Vec<NormalizedSample>,
    }
    impl TrackPackager for HoldingWriter {
        fn track_id(&self) -> crate::domain::TrackId {
            self.id
        }
        fn buffered(&self) -> (usize, usize) {
            (
                self.samples
                    .iter()
                    .map(NormalizedSample::retained_bytes)
                    .sum(),
                self.samples.len(),
            )
        }
        fn push(
            &mut self,
            sample: NormalizedSample,
            _: &mut dyn Appender<PackagedMedia>,
        ) -> Result<(), MuxError> {
            self.samples.push(sample);
            Ok(())
        }
        fn finish(
            &mut self,
            _: FinishReason,
            _: &mut dyn Appender<PackagedMedia>,
        ) -> Result<(), MuxError> {
            self.samples.clear();
            Ok(())
        }
    }

    #[test]
    fn writer_windows_share_one_publication_budget() -> Result<(), Box<dyn std::error::Error>> {
        for byte_limit in [false, true] {
            let (mut mux, _) = coordinator(0, 0)?;
            for track in &mut mux.tracks {
                track.writer = Box::new(HoldingWriter {
                    id: track.plan.track_id,
                    samples: Vec::new(),
                });
            }
            if byte_limit {
                mux.limits.maximum_buffered_bytes = 3 * sample(0, 0, true).retained_bytes();
            } else {
                mux.limits.maximum_buffered_samples = 3;
            }
            let mut output = Vec::new();
            for id in 0..3 {
                mux.push(sample(id, 0, true), &mut output)?;
            }
            assert_eq!(
                mux.samples, 0,
                "all input moved from coordinator queues into writers"
            );
            assert!(matches!(
                mux.push(sample(0, 40, false), &mut output),
                Err(MuxError::CoordinatorLimit { samples: 4, .. })
            ));
            mux.finish(FinishReason::Superseded, &mut output)?;
            assert!(
                mux.tracks
                    .iter()
                    .all(|track| track.writer.buffered() == (0, 0))
            );
        }
        Ok(())
    }
}
