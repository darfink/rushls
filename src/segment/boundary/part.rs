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
    choose_part_duration(&access_unit_durations, segment_duration, desired)
}

/// Chooses the access-unit count whose window duration sits nearest `desired`.
///
/// # Why this is not an exhaustive scan
///
/// Measuring every count against every window is quadratic, and the count is
/// bounded only by
/// [`PrerollLimits::maximum_buffered_samples`](crate::segment::PrerollLimits::maximum_buffered_samples)
/// — sixteen thousand by default. A publisher sending very short access units
/// reaches that cheaply, so an exhaustive search hands it a large, free slice
/// of a core once per session.
///
/// The search is pruned instead, resting on one property: the **longest**
/// window is non-decreasing in the count. Every window of `k + 1` access units
/// contains one of `k`, and durations are unsigned, so widening the window can
/// only raise the maximum. Three consequences follow, and each is a bisection
/// or an early exit rather than a scan:
///
/// - Counts whose windows overrun the segment form a suffix, so the usable
///   range has a bisectable upper bound.
/// - Counts whose windows are empty form a prefix, so it has a bisectable
///   lower bound. This is what stops an input of zero-duration access units
///   from costing the full quadratic sweep before being rejected.
/// - Distance from `desired` is therefore V-shaped across the range, so
///   walking outward from its floor can stop as soon as the distance exceeds
///   the best candidate already accepted.
///
/// The 85% consistency rule is *not* monotone, so it can never terminate the
/// walk — only the distance can. That is why the pruning is expressed in terms
/// of distance alone.
fn choose_part_duration(
    durations: &[TickDuration],
    segment_duration: TickDuration,
    desired: TickDuration,
) -> Option<NonZero<TickDuration>> {
    let largest = largest_fitting_count(durations, segment_duration)?;
    if largest == 0 {
        return None;
    }
    // A window of zero duration is never a part target, and those counts are a
    // prefix, so the range starts after them.
    let smallest = first_count_reaching(durations, largest, 1)?;
    if smallest > largest {
        return None;
    }
    let usable = smallest..=largest;
    // The floor of the distance curve is at this count or the one below it.
    // Walking outward from here covers both.
    let pivot = first_count_reaching(durations, largest, desired)?.clamp(smallest, largest);

    let mut best = None;
    // Downward first: on a tie the shorter target wins, and that is the
    // direction shorter targets lie in.
    scan(
        durations,
        (smallest..=pivot).rev(),
        segment_duration,
        desired,
        &mut best,
    )?;
    scan(
        durations,
        pivot.saturating_add(1)..=*usable.end(),
        segment_duration,
        desired,
        &mut best,
    )?;

    best.and_then(|candidate| NonZero::new(candidate.target))
}

/// Measures counts in the given order until the distance can only grow.
///
/// The caller walks outward from the floor of the distance curve, so within one
/// direction the distance is monotone. The comparison is deliberately strict:
/// an equal distance keeps the walk going, because those are decided by the
/// shorter target and the downward walk is still finding shorter ones.
fn scan(
    durations: &[TickDuration],
    counts: impl Iterator<Item = usize>,
    segment_duration: TickDuration,
    desired: TickDuration,
    best: &mut Option<PartDurationCandidate>,
) -> Option<()> {
    for access_unit_count in counts {
        let observed = measure_windows(durations, access_unit_count)?;
        let distance_from_desired = observed.longest.abs_diff(desired);
        if best.is_some_and(|current| distance_from_desired > current.distance_from_desired) {
            break;
        }
        if !is_usable_candidate(observed, segment_duration) {
            continue;
        }

        let candidate = PartDurationCandidate {
            target: observed.longest,
            distance_from_desired,
        };
        if candidate.is_better_than(*best) {
            *best = Some(candidate);
        }
    }
    Some(())
}

/// The largest count whose longest window still fits one segment, or zero when
/// even a single access unit overruns it.
fn largest_fitting_count(
    durations: &[TickDuration],
    segment_duration: TickDuration,
) -> Option<usize> {
    let mut low = 1;
    let mut high = durations.len();
    let mut fitting = 0;
    while low <= high {
        let middle = low + (high - low) / 2;
        if measure_windows(durations, middle)?.longest <= segment_duration {
            fitting = middle;
            low = middle + 1;
        } else {
            high = middle - 1;
        }
    }
    Some(fitting)
}

/// The smallest count up to `limit` whose longest window reaches `threshold`,
/// or `limit + 1` when none does.
fn first_count_reaching(
    durations: &[TickDuration],
    limit: usize,
    threshold: TickDuration,
) -> Option<usize> {
    let mut low = 1;
    let mut high = limit;
    let mut reaching = limit + 1;
    while low <= high {
        let middle = low + (high - low) / 2;
        if measure_windows(durations, middle)?.longest >= threshold {
            reaching = middle;
            high = middle - 1;
        } else {
            low = middle + 1;
        }
    }
    Some(reaching)
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

    /// The unpruned search, kept as the reference the pruned one is checked
    /// against.
    ///
    /// This is the algorithm `choose_part_duration` replaced, preserved
    /// verbatim. Its only job is to be obviously correct — every count, every
    /// window, no reasoning about monotonicity — so that any disagreement is
    /// attributable to the pruning rather than to a second clever
    /// implementation.
    fn exhaustive_part_duration(
        durations: &[TickDuration],
        segment_duration: TickDuration,
        desired: TickDuration,
    ) -> Option<NonZero<TickDuration>> {
        let mut best_candidate = None;
        for access_unit_count in 1..=durations.len() {
            let observed = measure_windows(durations, access_unit_count)?;
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

    /// Deterministic xorshift, so a failure is reproducible from the seed.
    fn generator(seed: u64) -> impl FnMut() -> u64 {
        let mut state = seed;
        move || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        }
    }

    #[test]
    fn the_pruned_search_agrees_with_an_exhaustive_one() {
        let mut next = generator(0x2545_F491_4F6C_DD1D);
        for case in 0..2_000 {
            let count = (next() % 40) as usize;
            // Vary the spread so cases land on both sides of the 85% cadence
            // rule, and include zero durations, which are the prefix the lower
            // bound exists to skip.
            let spread = 1 + next() % 400;
            let durations: Vec<TickDuration> = (0..count).map(|_| next() % spread).collect();
            let segment_duration = next() % 4_000;
            let desired = next() % 1_500;

            assert_eq!(
                choose_part_duration(&durations, segment_duration, desired),
                exhaustive_part_duration(&durations, segment_duration, desired),
                "case {case}: durations={durations:?}, \
                 segment_duration={segment_duration}, desired={desired}"
            );
        }
    }

    #[test]
    fn uniform_cadences_agree_with_an_exhaustive_search() {
        // The realistic shape: a fixed frame duration, so every count has
        // shortest == longest and the cadence rule never rejects. This is the
        // case the outward walk prunes hardest, so it is worth pinning
        // separately from the noisy one.
        for duration in [1_501_u64, 3_000, 3_003, 48_000] {
            for count in [1_usize, 2, 7, 30, 61] {
                let durations = vec![duration; count];
                for desired in [0_u64, 1, 18_000, 90_000, u64::MAX] {
                    let segment_duration = duration * 30;
                    assert_eq!(
                        choose_part_duration(&durations, segment_duration, desired),
                        exhaustive_part_duration(&durations, segment_duration, desired),
                        "duration={duration}, count={count}, desired={desired}"
                    );
                }
            }
        }
    }

    #[test]
    fn zero_duration_access_units_are_rejected_without_an_exhaustive_sweep() {
        // The cheap hostile input: thousands of access units that advance the
        // clock but carry no duration. Every count measures zero, so none is a
        // usable part target — the point is that finding that out no longer
        // costs a quadratic sweep.
        let durations = vec![0_u64; 16_384];

        assert_eq!(choose_part_duration(&durations, 540_000, 90_000), None);
    }

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
