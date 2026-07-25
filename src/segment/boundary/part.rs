use std::num::NonZero;

use crate::{
    domain::{TickDuration, TickTimestamp, TrackId, duration_since},
    media::NormalizedSample,
};

/// The shortest observed window must be this close to the longest window.
///
/// This rejects an access-unit count when variable sample durations would
/// produce regular parts that differ too much in duration.
const MINIMUM_CADENCE_CONSISTENCY_PERCENT: u128 = 85;

#[derive(Clone, Copy)]
struct WindowDurationRange {
    shortest: TickDuration,
    longest: TickDuration,
}

#[derive(Clone, Copy)]
struct PartDurationCandidate {
    target: TickDuration,
    distance_from_desired: TickDuration,
}

impl PartDurationCandidate {
    fn is_better_than(self, current: Option<Self>) -> bool {
        current.is_none_or(|current| {
            self.distance_from_desired < current.distance_from_desired
                || (self.distance_from_desired == current.distance_from_desired
                    && self.target < current.target)
        })
    }
}

/// Selects a realizable regular-part target near the configured preference.
///
/// Each candidate represents a fixed number of consecutive access units. Its
/// target is the longest duration observed for that count. A candidate is
/// accepted only when:
///
/// - its longest window fits within the segment; and
/// - its shortest window is at least 85% as long as its longest.
///
/// Among accepted candidates, the target closest to `desired` wins. Ties favor
/// the shorter target. The final, truncated part is deliberately excluded from
/// this cadence check because it is allowed to be shorter than regular parts.
pub(in crate::segment) fn select_part_duration(
    samples: &[NormalizedSample],
    track_id: TrackId,
    origin: TickTimestamp,
    segment_boundary: TickTimestamp,
    desired: TickDuration,
) -> Option<NonZero<TickDuration>> {
    let access_unit_durations: Vec<_> = samples
        .iter()
        .filter(|sample| {
            sample.track_id() == track_id
                && origin <= sample.pts()
                && sample.pts() < segment_boundary
        })
        .map(NormalizedSample::duration)
        .collect();
    let segment_duration = duration_since(segment_boundary, origin)?;
    let mut best_candidate = None;

    for access_unit_count in 1..=access_unit_durations.len() {
        let observed = measure_windows(&access_unit_durations, access_unit_count)?;

        if !is_usable_candidate(observed, segment_duration) {
            continue;
        }

        let candidate = PartDurationCandidate {
            target: observed.longest,
            distance_from_desired: observed.longest.abs_diff(desired),
        };
        if candidate.is_better_than(best_candidate) {
            best_candidate = Some(candidate);
        }
    }

    best_candidate.and_then(|candidate| NonZero::new(candidate.target))
}

/// Measures every consecutive window containing `access_unit_count` samples.
fn measure_windows(
    durations: &[TickDuration],
    access_unit_count: usize,
) -> Option<WindowDurationRange> {
    let mut window_duration = durations[..access_unit_count]
        .iter()
        .try_fold(0_u64, |sum, duration| sum.checked_add(*duration))?;
    let mut observed = WindowDurationRange {
        shortest: window_duration,
        longest: window_duration,
    };

    // Slide the window in O(n): remove the sample that leaves the window and
    // add the one that enters it.
    for start in 1..=durations.len() - access_unit_count {
        window_duration = window_duration
            .checked_sub(durations[start - 1])?
            .checked_add(durations[start + access_unit_count - 1])?;
        observed.shortest = observed.shortest.min(window_duration);
        observed.longest = observed.longest.max(window_duration);
    }

    Some(observed)
}

fn is_usable_candidate(observed: WindowDurationRange, segment_duration: TickDuration) -> bool {
    if observed.longest == 0 || observed.longest > segment_duration {
        return false;
    }

    // Widen before multiplying so a valid u64 duration cannot overflow.
    u128::from(observed.shortest) * 100
        >= u128::from(observed.longest) * MINIMUM_CADENCE_CONSISTENCY_PERCENT
}

#[cfg(test)]
mod tests {
    use crate::{
        domain::{Codec, Payload, TrackId},
        media::VideoSample,
    };

    use super::*;

    fn samples(durations: &[u64]) -> Vec<NormalizedSample> {
        let mut pts = 0_i64;
        durations
            .iter()
            .map(|duration| {
                let sample = NormalizedSample::Video(VideoSample {
                    track_id: TrackId(0),
                    codec: Codec::H264,
                    pts,
                    dts: pts,
                    duration: *duration,
                    random_access: false,
                    payload: Payload::default(),
                });
                pts += i64::try_from(*duration).expect("test duration fits i64");
                sample
            })
            .collect()
    }

    #[test]
    fn selects_six_frames_for_2997_fps_near_200ms() {
        let samples = samples(&vec![3_003; 60]);

        assert_eq!(
            select_part_duration(&samples, TrackId(0), 0, 180_180, 18_000,),
            NonZero::new(18_018)
        );
    }

    #[test]
    fn selects_nine_aac_access_units_near_200ms() {
        let samples = samples(&vec![1_024; 100]);

        assert_eq!(
            select_part_duration(&samples, TrackId(0), 0, 102_400, 9_600,),
            NonZero::new(9_216)
        );
    }

    #[test]
    fn uses_maximum_observed_window_as_safe_variable_cadence_target() {
        let samples = samples(&[100, 110, 100, 110, 100, 110]);

        assert_eq!(
            select_part_duration(&samples, TrackId(0), 0, 630, 410,),
            NonZero::new(420)
        );
    }

    #[test]
    fn does_not_require_part_target_to_divide_segment_duration() {
        let samples = samples(&[20, 20, 20, 20, 20, 20]);

        assert_eq!(
            select_part_duration(&samples, TrackId(0), 0, 110, 60,),
            NonZero::new(60)
        );
    }

    #[test]
    fn requires_cadence_evidence_before_the_segment_boundary() {
        assert_eq!(select_part_duration(&[], TrackId(0), 0, 100, 20,), None);
    }
}
