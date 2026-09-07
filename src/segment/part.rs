//! Choosing how many access units make up a regular part.
//!
//! # Why parts are counted rather than scheduled
//!
//! Segments use close track-local schedules selected together during pre-roll.
//! Parts are also rendition-local, and they carry a constraint segments do not:
//! HLS requires every part to be no longer than
//! `PART-TARGET`, and every part except the last of its segment to reach 85% of
//! it. Delivery enforces both — see `permits_part` and `permits_non_final_part`
//! in `crate::delivery::hls::store` — so a plan that cannot guarantee them
//! fails the publication once media is already flowing.
//!
//! A tick schedule cannot guarantee the ceiling. A cut lands on the first
//! access unit at or after the planned instant, so an achieved part can
//! overshoot its target by up to one access unit. Counting instead makes both
//! bounds structural: a regular part is exactly `n` presentable access units,
//! so it lasts between `n × shortest` and `n × longest`, and advertising
//! `n × longest` as the target puts the ceiling beyond reach by definition.
//! The floor then reduces to a property of the access units themselves —
//! `shortest ≥ 85% × longest` — which is a comparison rather than a search.
//!
//! Durations describe the encoded grid, before audio trim. The first audible
//! unit can be partially primed; CMAF retains its encoded start as the chunk
//! origin. Fully primed units are not counted. Constant encoded durations
//! therefore produce regular parts exactly on target, even with pre-skip.
//!
//! # Balanced remainders
//!
//! Final parts may legally fall below the 85% floor, but very short parts are a
//! poor interoperability bet. When a nearby access-unit count avoids one, it is
//! preferred without making short remainders invalid. For example, 93 AAC
//! frames use `24 + 24 + 24 + 21` instead of `23 + 23 + 23 + 23 + 1`.

use std::num::NonZero;

use crate::domain::{MediaKind, TickDuration, TrackId};

use super::CadenceError;

/// The fraction of the longest access unit the shortest must reach.
///
/// Mirrors the `PART-TARGET` floor delivery enforces. Checking it here, on
/// individual access units, is deliberately conservative: a part sums `n` of
/// them, so averaging can only narrow the spread this admits.
const MINIMUM_CADENCE_CONSISTENCY_PERCENT: u128 = 85;

/// Preferred minimum final-part fraction of `PART-TARGET`.
///
/// Final parts may be shorter; this only ranks nearby valid grids.
const PREFERRED_FINAL_PART_PERCENT: u128 = 85;

/// Search radius around the nearest access-unit count.
const REMAINDER_SEARCH_RADIUS: u32 = 1;

/// The observed spread of encoded durations for units with presentable media.
///
/// `shortest` is absent until the track presents something. A fully primed
/// access unit contributes nothing, which is what keeps codec delay from
/// describing a grid it is not part of.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct AccessUnitCadence {
    shortest: Option<TickDuration>,
    longest: TickDuration,
}

impl AccessUnitCadence {
    pub fn observe(&mut self, duration: TickDuration) {
        self.shortest = Some(
            self.shortest
                .map_or(duration, |current| current.min(duration)),
        );
        self.longest = self.longest.max(duration);
    }

    pub fn longest(self) -> TickDuration {
        self.longest
    }

    fn is_consistent(self) -> bool {
        // Widen before multiplying so a valid u64 duration cannot overflow.
        self.shortest.is_some_and(|shortest| {
            u128::from(shortest) * 100
                >= u128::from(self.longest) * MINIMUM_CADENCE_CONSISTENCY_PERCENT
        })
    }

    /// Whether the final-part remainder can be predicted from the segment grid.
    fn is_constant(self) -> bool {
        self.shortest == Some(self.longest)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PartCadence {
    pub access_units: NonZero<u32>,
    /// The advertised `PART-TARGET`: `access_units × the longest access unit`.
    pub duration: NonZero<TickDuration>,
}

/// Selects the access-unit count whose span sits nearest the desired target.
///
/// The count is clamped so a part is never empty and a regular part never
/// exceeds one segment.
pub fn select_part_cadence(
    track_id: TrackId,
    kind: MediaKind,
    access_units: AccessUnitCadence,
    desired: TickDuration,
    segment: TickDuration,
) -> Result<PartCadence, CadenceError> {
    // A subtitle track publishes whole segments — a WebVTT rendition sets
    // `chunk_target` to `None` — and a cue's duration measures how long text
    // stays on screen, not a grid anything is cut on. Sizing parts from cues
    // would make one short caption reject the publication, so subtitles get a
    // nominal cadence and are never asked to have produced media at all.
    if kind == MediaKind::Subtitle {
        return Ok(PartCadence {
            access_units: nz::u32!(1),
            duration: NonZero::new(desired.min(segment))
                .or(NonZero::new(segment))
                .ok_or(CadenceError::NoSegmentationBoundary)?,
        });
    }
    let longest =
        NonZero::new(access_units.longest).ok_or(CadenceError::NoPresentableMedia(track_id))?;
    if !access_units.is_consistent() {
        return Err(CadenceError::InconsistentPartCadence(track_id));
    }

    let nearest = desired
        .saturating_add(longest.get() / 2)
        .checked_div(longest.get())
        .unwrap_or(1);
    let count = nearest.clamp(1, (segment / longest).max(1));
    let count = u32::try_from(count).unwrap_or(u32::MAX);
    let count = balance_final_part(count, access_units, longest.get(), segment);
    let access_units = NonZero::new(count).unwrap_or(nz::u32!(1));
    let duration = u64::from(access_units.get())
        .checked_mul(longest.get())
        .and_then(NonZero::new)
        .ok_or(CadenceError::HorizonOverflow(track_id))?;
    Ok(PartCadence {
        access_units,
        duration,
    })
}

/// Prefers an adjacent count with no short final-part remainder.
///
/// Variable or non-integral grids keep `nearest` because their remainder is not
/// predictable in advance.
fn balance_final_part(
    nearest: u32,
    access_units: AccessUnitCadence,
    longest: TickDuration,
    segment: TickDuration,
) -> u32 {
    if !access_units.is_constant() || !segment.is_multiple_of(longest) {
        return nearest;
    }
    let units_per_segment = segment / longest;
    if units_per_segment == 0 {
        return nearest;
    }

    // Prefer no remainder, or one large enough to resemble a regular part.
    let leaves_usable_remainder = |count: u32| {
        let remainder = units_per_segment % TickDuration::from(count);
        remainder == 0
            || u128::from(remainder) * 100 >= u128::from(count) * PREFERRED_FINAL_PART_PERCENT
    };

    if leaves_usable_remainder(nearest) {
        return nearest;
    }

    let maximum = u32::try_from(units_per_segment).unwrap_or(u32::MAX).max(1);
    // Keep the achieved part duration as close as possible to the request.
    (nearest.saturating_sub(REMAINDER_SEARCH_RADIUS).max(1)
        ..=nearest.saturating_add(REMAINDER_SEARCH_RADIUS).min(maximum))
        .filter(|candidate| leaves_usable_remainder(*candidate))
        .min_by_key(|candidate| candidate.abs_diff(nearest))
        .unwrap_or(nearest)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cadence(durations: &[TickDuration]) -> AccessUnitCadence {
        let mut cadence = AccessUnitCadence::default();
        for duration in durations {
            cadence.observe(*duration);
        }
        cadence
    }

    fn select(
        durations: &[TickDuration],
        desired: TickDuration,
        segment: TickDuration,
    ) -> Result<PartCadence, CadenceError> {
        select_part_cadence(
            TrackId(0),
            MediaKind::Video,
            cadence(durations),
            desired,
            segment,
        )
    }

    fn select_audio(
        durations: &[TickDuration],
        desired: TickDuration,
        segment: TickDuration,
    ) -> Result<PartCadence, CadenceError> {
        select_part_cadence(
            TrackId(0),
            MediaKind::Audio,
            cadence(durations),
            desired,
            segment,
        )
    }

    #[test]
    fn selects_six_frames_for_2997_fps_near_200ms() {
        assert_eq!(
            select(&[3_003; 60], 18_000, 180_180),
            Ok(PartCadence {
                access_units: nz::u32!(6),
                duration: nz::u64!(18_018),
            })
        );
    }

    #[test]
    fn selects_ten_aac_access_units_near_200ms() {
        // Nine-frame parts leave one frame; ten-frame parts divide exactly.
        assert_eq!(
            select(&[1_024; 100], 9_600, 102_400),
            Ok(PartCadence {
                access_units: nz::u32!(10),
                duration: nz::u64!(10_240),
            })
        );
    }

    #[test]
    fn a_one_frame_final_part_is_widened_away() {
        // Move one frame from the target to avoid a one-frame remainder.
        assert_eq!(
            select_audio(&[1_024; 93], 23_552, 95_232),
            Ok(PartCadence {
                access_units: nz::u32!(24),
                duration: nz::u64!(24_576),
            })
        );
    }

    #[test]
    fn an_exact_division_is_left_alone() {
        // An exact nearest grid needs no adjustment.
        assert_eq!(
            select_audio(&[1_024; 96], 24_576, 98_304),
            Ok(PartCadence {
                access_units: nz::u32!(24),
                duration: nz::u64!(24_576),
            })
        );
    }

    #[test]
    fn a_final_part_already_at_the_floor_is_left_alone() {
        // A 19-of-20 remainder already exceeds the preferred floor.
        assert_eq!(
            select_audio(&[1_000; 119], 20_000, 119_000),
            Ok(PartCadence {
                access_units: nz::u32!(20),
                duration: nz::u64!(20_000),
            })
        );
    }

    #[test]
    fn a_short_final_part_remains_legal_when_nothing_nearby_is_better() {
        // Short final parts remain valid when adjacent grids do not improve it.
        assert_eq!(
            select_audio(&[1_024; 21], 5_120, 21_504),
            Ok(PartCadence {
                access_units: nz::u32!(5),
                duration: nz::u64!(5_120),
            })
        );
    }

    #[test]
    fn a_variable_cadence_keeps_the_nearest_count() {
        // A variable cadence has no predictable remainder to balance.
        assert_eq!(
            select(&[100, 110, 100, 110, 100, 110], 410, 630),
            Ok(PartCadence {
                access_units: nz::u32!(4),
                duration: nz::u64!(440),
            })
        );
    }

    #[test]
    fn a_non_integral_segment_grid_keeps_the_nearest_count() {
        // A non-integral grid may yield different unit counts across segments.
        assert_eq!(
            select_audio(&[20; 9], 60, 170),
            Ok(PartCadence {
                access_units: nz::u32!(3),
                duration: nz::u64!(60),
            })
        );
    }

    #[test]
    fn the_target_is_a_ceiling_over_a_variable_cadence() {
        // Four units span at most 440 ticks here, so that is what the playlist
        // must promise: a target of 420 would be violated by the very first
        // window that happened to contain two long units.
        assert_eq!(
            select(&[100, 110, 100, 110, 100, 110], 410, 630),
            Ok(PartCadence {
                access_units: nz::u32!(4),
                duration: nz::u64!(440),
            })
        );
    }

    #[test]
    fn a_cadence_too_uneven_to_hold_the_85_percent_floor_is_rejected() {
        assert_eq!(
            select(&[100, 20, 100], 200, 1_000),
            Err(CadenceError::InconsistentPartCadence(TrackId(0)))
        );
        // Exactly at the floor, which is admissible.
        assert!(select(&[100, 85, 100], 200, 1_000).is_ok());
    }

    #[test]
    fn the_part_target_need_not_divide_the_segment() {
        assert_eq!(
            select(&[20; 6], 60, 110),
            Ok(PartCadence {
                access_units: nz::u32!(3),
                duration: nz::u64!(60),
            })
        );
    }

    #[test]
    fn a_regular_part_never_outlasts_its_segment() {
        // A one-second preference against a two-second segment of 500-tick
        // units: the count is capped rather than promising a part the segment
        // cannot contain.
        assert_eq!(
            select(&[500; 8], 4_000, 2_000),
            Ok(PartCadence {
                access_units: nz::u32!(4),
                duration: nz::u64!(2_000),
            })
        );
    }

    #[test]
    fn a_part_is_never_empty() {
        assert_eq!(
            select(&[3_000; 4], 0, 12_000),
            Ok(PartCadence {
                access_units: nz::u32!(1),
                duration: nz::u64!(3_000),
            })
        );
    }

    #[test]
    fn a_track_that_presented_nothing_cannot_be_given_a_part_cadence() {
        assert_eq!(
            select(&[], 18_000, 180_000),
            Err(CadenceError::NoPresentableMedia(TrackId(0)))
        );
        // Subtitles publish whole segments, so they are planned regardless.
        assert_eq!(
            select_part_cadence(
                TrackId(0),
                MediaKind::Subtitle,
                AccessUnitCadence::default(),
                18_000,
                180_000,
            ),
            Ok(PartCadence {
                access_units: nz::u32!(1),
                duration: nz::u64!(18_000),
            })
        );
    }

    #[test]
    fn wildly_uneven_subtitle_cues_do_not_reject_a_publication() {
        // Cue durations measure how long text stays on screen. A two-word
        // caption beside a full sentence is ordinary subtitling, and judging it
        // as an access-unit grid would fail a session over nothing.
        assert_eq!(
            select_part_cadence(
                TrackId(0),
                MediaKind::Subtitle,
                cadence(&[90_000, 900_000, 45_000]),
                90_000,
                540_000,
            ),
            Ok(PartCadence {
                access_units: nz::u32!(1),
                duration: nz::u64!(90_000),
            })
        );
    }

    #[test]
    fn fully_primed_access_units_do_not_describe_the_grid() {
        // `observe` is only ever called for units with presentable media, so a track
        // whose leading units are entirely codec delay still reports the real
        // frame length once audible media starts.
        let mut cadence = AccessUnitCadence::default();
        cadence.observe(1_024);

        assert_eq!(cadence.longest(), 1_024);
        assert!(cadence.is_consistent());
    }
}
