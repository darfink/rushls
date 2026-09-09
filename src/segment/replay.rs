//! Explicit admission through the live coordinator and timing-only CMAF writers.
use super::{CadenceError, PrerollError, SegmentationPlan, SegmentationPolicy};
use crate::{
    media::{NormalizedSample, PresentationPlan, PresentedTimingCursor},
    mux::{self, MuxError},
};
use std::{collections::BTreeSet, num::NonZero};

/// Choose the smallest feasible ceiling without guessing where live segments cut.
/// Candidate replays share a work budget as well as the coordinator's memory bounds.
pub fn admit(
    presentation: &PresentationPlan,
    plan: &mut SegmentationPlan,
    samples: &[NormalizedSample],
    policy: SegmentationPolicy,
    work: &mut usize,
) -> Result<(), PrerollError> {
    for track in plan.iter() {
        if policy.early_boundary
            >= track
                .timebase
                .ticks_to_duration(track.segment_duration.get())
            || track.timebase.ticks_to_duration(
                track.maximum_segment_ticks(
                    policy.early_boundary.saturating_add(policy.late_boundary),
                ),
            ) > policy.segment_cap()
        {
            return Err(CadenceError::NoSegmentationBoundary.into());
        }
    }
    loop {
        *work = work.saturating_add(samples.len());
        if *work > 1_000_000 {
            return Err(PrerollError::LimitExceeded);
        }
        match mux::validate_timing(presentation, plan, samples) {
            Ok(()) => return Ok(()),
            Err(MuxError::Part { track, .. }) => {
                let index = plan
                    .tracks
                    .iter()
                    .position(|p| p.track_id == track)
                    .ok_or(CadenceError::UnknownTrack(track))?;
                let selected = plan.tracks[index];
                let source = presentation
                    .catalog()
                    .get(track)
                    .ok_or(CadenceError::UnknownTrack(track))?;
                let mut cursor = PresentedTimingCursor::for_track(source);
                let mut durations = Vec::new();
                let mut clock =
                    super::cutter::PartClock::new(source.kind(), selected.segmentation_origin_pts);
                for sample in samples.iter().filter(|s| s.track_id() == track) {
                    let timing = cursor.next(sample).map_err(|source| {
                        CadenceError::InvalidSampleTiming {
                            track_id: track,
                            source,
                        }
                    })?;
                    if timing.duration > 0 {
                        durations.push(
                            clock
                                .advance(timing.start, timing.duration)
                                .ok_or(CadenceError::TimestampOverflow(track))?,
                        );
                    }
                }
                let maximum = selected
                    .timebase
                    .duration_to_ticks_floor(policy.maximum_part_duration)
                    .min(selected.segment_duration.get());
                let candidates =
                    candidates(&durations, selected.part_duration.get(), maximum, work)?;
                let mut found = false;
                for ceiling in candidates {
                    *work = work.saturating_add(samples.len());
                    if *work > 1_000_000 {
                        return Err(PrerollError::LimitExceeded);
                    }
                    plan.tracks[index].part_duration =
                        NonZero::new(ceiling).ok_or(CadenceError::InvalidPolicy)?;
                    match mux::validate_timing(presentation, plan, samples) {
                        Err(MuxError::Part { track: failed, .. }) if failed == track => {}
                        _ => {
                            found = true;
                            break;
                        }
                    }
                }
                if !found {
                    return Err(CadenceError::InconsistentPartCadence(track).into());
                }
            }
            Err(error) => return Err(PrerollError::Packaging(error)),
        }
    }
}

fn candidates(
    durations: &[u64],
    preference: u64,
    maximum: u64,
    work: &mut usize,
) -> Result<BTreeSet<u64>, PrerollError> {
    let mut candidates = BTreeSet::new();
    if maximum > preference {
        candidates.insert(maximum);
    }
    for start in 0..durations.len() {
        let mut sum = 0_u64;
        for duration in &durations[start..] {
            *work = work.saturating_add(1);
            if *work > 1_000_000 {
                return Err(PrerollError::LimitExceeded);
            }
            let Some(next) = sum.checked_add(*duration) else {
                break;
            };
            sum = next;
            if sum > maximum {
                break;
            }
            let threshold = u64::try_from(u128::from(sum) * 100 / 85).unwrap_or(u64::MAX);
            for value in [sum, threshold, threshold.saturating_add(1)] {
                if preference < value && value <= maximum {
                    candidates.insert(value);
                }
            }
        }
    }
    Ok(candidates)
}
