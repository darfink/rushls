//! The media-playlist invariants a rendition is frozen to at creation.
//!
//! `EXT-X-TARGETDURATION` and `PART-TARGET` must not change while a media
//! playlist exists, and every segment and part a playlist names has to satisfy
//! them. Both facts point the same way: these values cannot be tracked from
//! observed media, because by the time media proves the current value wrong, it
//! has already been published under it. They are therefore derived once, from
//! the muxer's declared output configuration, and held immutable for the
//! rendition's life.
//!
//! The target duration is a property of the *presentation* rather than of one
//! rendition: the protocol requires every media playlist in a multivariant
//! playlist to advertise the same value. It is therefore supplied to
//! [`PlaylistContract::derive`] rather than computed from one configuration,
//! so a rendition cannot be given a contract that disagrees with its siblings.
//!
//! A new publication with an incompatible contract receives a different durable
//! rendition. An active publication cannot change its contract: timing failures
//! end it rather than silently changing terms under existing viewers.

use std::{num::NonZeroU64, time::Duration};

use crate::domain::duration_from_nanos_saturating;
use crate::mux::{MediaSegmentFormat, RenditionConfig};

/// The fraction of `PART-TARGET` a part must reach while it is not the last
/// part of its segment.
const MINIMUM_NON_FINAL_PART_PERCENT: u128 = 85;

/// What one media playlist promises for as long as it exists.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PlaylistContract {
    /// The `EXT-X-TARGETDURATION` value, in whole seconds.
    pub target_duration: NonZeroU64,
    /// The exact bound every segment must respect, before rounding.
    ///
    /// Retained separately from [`Self::target_duration`] so the muxer's
    /// advertised budget and the rejection message stay exact. Validation
    /// itself follows the protocol: the *segment's* duration is rounded to
    /// the nearest whole second and compared to the target, exactly as
    /// section 4.4.3.1 of draft-pantos-hls-rfc8216bis requires.
    pub maximum_segment_duration: Duration,
    /// The `PART-TARGET` value, absent for a rendition publishing whole
    /// segments.
    pub part_target: Option<Duration>,
    pub segment_format: MediaSegmentFormat,
}

impl PlaylistContract {
    /// The one `EXT-X-TARGETDURATION` every media playlist of a presentation
    /// advertises, or `None` for a presentation with no renditions.
    ///
    /// Section 6.2.4 of draft-pantos-hls-rfc8216bis requires that "each Media
    /// Playlist in each Variant Stream and Rendition MUST have the same Target
    /// Duration". Its exceptions cover SUBTITLES renditions and I-frames-only
    /// playlists only when they carry an `EXT-X-PLAYLIST-TYPE` of VOD, which a
    /// live origin never publishes, so every rendition is included here.
    ///
    /// The shared value is the largest rendition requirement, because each
    /// rendition's own rounded ceiling is a floor rather than a preference: a
    /// smaller target would refuse the longest segment that rendition's muxer
    /// has already been permitted to emit.
    pub fn presentation_target_duration<'a>(
        configs: impl IntoIterator<Item = &'a RenditionConfig>,
    ) -> Option<NonZeroU64> {
        configs
            .into_iter()
            .map(|config| nearest_whole_seconds(longest_segment(config)))
            .max()
    }

    /// Freezes one rendition's contract against the presentation-wide
    /// `target_duration` from [`Self::presentation_target_duration`].
    ///
    /// Reject an inconsistent presentation ceiling; never silently change one
    /// sibling's target independently of the others.
    pub fn derive(config: &RenditionConfig, target_duration: NonZeroU64) -> Option<Self> {
        let maximum_segment_duration = longest_segment(config);
        if target_duration < nearest_whole_seconds(maximum_segment_duration) {
            return None;
        }
        Some(Self {
            target_duration,
            maximum_segment_duration,
            part_target: config
                .chunk_target
                .map(|target| config.timebase.ticks_to_duration(target.get())),
            segment_format: config.segment_format,
        })
    }

    /// Whether the rendition publishes partial segments at all.
    pub fn is_chunked(&self) -> bool {
        self.part_target.is_some()
    }

    pub fn permits_segment(&self, duration: Duration) -> bool {
        nearest_whole_seconds(duration) <= self.target_duration
    }

    /// Whether a part is short enough to be tagged at all.
    ///
    /// A rendition with no part target has nothing to check: it never
    /// publishes parts, and a chunk arriving for it is refused earlier as a
    /// packaging-mode error rather than a duration one.
    pub fn permits_part(&self, duration: Duration) -> bool {
        self.part_target.is_none_or(|target| duration <= target)
    }

    /// The floor a part must reach once a successor makes it non-final.
    ///
    /// Computed exactly from integer nanoseconds rather than by scaling a
    /// float, so a target that is not a round number of milliseconds cannot
    /// produce a floor that rejects a part the muxer cut correctly.
    pub fn minimum_non_final_part_duration(&self) -> Option<Duration> {
        self.part_target.map(|target| {
            let nanos = target
                .as_nanos()
                .saturating_mul(MINIMUM_NON_FINAL_PART_PERCENT)
                / 100;
            duration_from_nanos_saturating(nanos)
        })
    }

    pub fn permits_non_final_part(&self, duration: Duration) -> bool {
        self.minimum_non_final_part_duration()
            .is_none_or(|minimum| duration >= minimum)
    }
}

/// The longest segment this rendition's muxer is permitted to emit.
fn longest_segment(config: &RenditionConfig) -> Duration {
    config
        .timebase
        .ticks_to_duration(config.maximum_segment_duration.get())
}

/// Rounds to the nearest whole second, never below one.
///
/// The protocol version 6 semantics of `EXT-X-TARGETDURATION` make the tag
/// "the maximum segment duration rounded to the nearest integer number of
/// seconds" (draft-pantos-hls-rfc8216bis, section 4.4.3.1), and the same
/// rounding is applied to each segment when it is judged against the target.
/// A sub-second cadence still needs a target of at least 1, which is the
/// smallest value the tag can carry.
fn nearest_whole_seconds(duration: Duration) -> NonZeroU64 {
    let seconds = duration.as_secs();
    let rounded = if duration.subsec_nanos() >= 500_000_000 {
        seconds.saturating_add(1)
    } else {
        seconds
    };
    NonZeroU64::new(rounded).unwrap_or(NonZeroU64::MIN)
}

#[cfg(test)]
mod tests {
    use crate::domain::Timebase;

    use super::*;

    fn config(maximum_ticks: u64, chunk_ticks: Option<u64>) -> RenditionConfig {
        RenditionConfig {
            timebase: Timebase::hz90k(),
            segment_target: nz::u64!(540_000),
            maximum_segment_duration: NonZeroU64::new(maximum_ticks).expect("nonzero"),
            chunk_target: chunk_ticks.map(|ticks| NonZeroU64::new(ticks).expect("nonzero")),
            segment_format: MediaSegmentFormat::Cmaf,
        }
    }

    /// The contract this rendition gets when it is the whole presentation.
    fn alone(config: &RenditionConfig) -> PlaylistContract {
        let target = PlaylistContract::presentation_target_duration([config])
            .expect("one rendition supplies a target");
        PlaylistContract::derive(config, target).expect("computed presentation target fits")
    }

    #[test]
    fn the_target_covers_the_longest_permitted_segment_after_rounding() {
        // Six seconds planned, with a one-second extension budget: the target
        // has to be seven, because a segment may legitimately reach 7 s.
        let contract = alone(&config(630_000, Some(90_000)));

        assert_eq!(contract.target_duration, nz::u64!(7));
        assert_eq!(contract.maximum_segment_duration, Duration::from_secs(7));
        assert!(contract.permits_segment(Duration::from_secs(7)));
        assert!(contract.permits_segment(Duration::from_millis(7_499)));
        assert!(!contract.permits_segment(Duration::from_millis(7_500)));
    }

    #[test]
    fn rounding_is_to_nearest_and_never_reaches_zero() {
        assert_eq!(
            alone(&config(585_000, None)).target_duration,
            nz::u64!(7),
            "6.5 s rounds up, so a 6.5 s segment still rounds within its target"
        );
        assert_eq!(alone(&config(584_999, None)).target_duration, nz::u64!(6));
        assert_eq!(
            alone(&config(9_000, None)).target_duration,
            nz::u64!(1),
            "a sub-second cadence still needs a representable target"
        );
    }

    #[test]
    fn segments_are_judged_by_their_rounded_duration_like_the_spec_requires() {
        let contract = alone(&config(540_000, None));

        assert_eq!(contract.target_duration, nz::u64!(6));
        assert!(
            contract.permits_segment(Duration::from_millis(6_499)),
            "6.499 s rounds to 6, within a target of 6"
        );
        assert!(
            !contract.permits_segment(Duration::from_millis(6_500)),
            "6.5 s rounds to 7, beyond a target of 6"
        );
    }

    #[test]
    fn non_final_parts_have_an_exact_eighty_five_percent_floor() {
        let contract = alone(&config(540_000, Some(90_000)));

        assert_eq!(
            contract.minimum_non_final_part_duration(),
            Some(Duration::from_millis(850))
        );
        assert!(contract.permits_non_final_part(Duration::from_millis(850)));
        assert!(!contract.permits_non_final_part(Duration::from_millis(849)));
        assert!(contract.permits_part(Duration::from_secs(1)));
        assert!(!contract.permits_part(Duration::from_millis(1_001)));
    }

    #[test]
    fn a_segment_only_rendition_constrains_nothing_about_parts() {
        let contract = alone(&config(540_000, None));

        assert!(!contract.is_chunked());
        assert_eq!(contract.minimum_non_final_part_duration(), None);
        assert!(contract.permits_part(Duration::from_mins(10)));
        assert!(contract.permits_non_final_part(Duration::ZERO));
    }

    #[test]
    fn one_target_duration_covers_every_rendition_in_the_presentation() {
        // The same 2.47 s cadence seen by two renditions whose boundary
        // tolerance differs: one AAC frame is about 21 ms and one 30 fps video
        // frame about 33 ms, which lands them either side of the half-second
        // rounding step. Derived per rendition they would advertise 2 and 3.
        let audio = config(224_190, None);
        let video = config(225_270, None);
        assert_eq!(nearest_whole_seconds(longest_segment(&audio)), nz::u64!(2));
        assert_eq!(nearest_whole_seconds(longest_segment(&video)), nz::u64!(3));

        let target = PlaylistContract::presentation_target_duration([&audio, &video])
            .expect("a presentation with renditions has a target");

        assert_eq!(target, nz::u64!(3), "the larger requirement admits both");
        assert_eq!(
            PlaylistContract::derive(&audio, target)
                .expect("shared target fits")
                .target_duration,
            PlaylistContract::derive(&video, target)
                .expect("shared target fits")
                .target_duration,
            "section 6.2.4 requires one value across the multivariant playlist"
        );
    }

    #[test]
    fn the_shared_target_is_independent_of_rendition_order() {
        let audio = config(224_190, None);
        let video = config(225_270, None);

        assert_eq!(
            PlaylistContract::presentation_target_duration([&audio, &video]),
            PlaylistContract::presentation_target_duration([&video, &audio])
        );
    }

    #[test]
    fn a_subtitle_rendition_shares_the_live_target_duration() {
        // The exception section 6.2.4 grants SUBTITLES renditions applies only
        // with an EXT-X-PLAYLIST-TYPE of VOD, which a live origin never
        // publishes, so a long cue segment raises the shared value.
        let video = config(180_000, Some(90_000));
        let subtitles = RenditionConfig {
            segment_format: MediaSegmentFormat::WebVtt,
            ..config(810_000, None)
        };

        let target = PlaylistContract::presentation_target_duration([&video, &subtitles])
            .expect("a presentation with renditions has a target");

        assert_eq!(target, nz::u64!(9));
        assert_eq!(
            PlaylistContract::derive(&video, target)
                .expect("shared target fits")
                .target_duration,
            nz::u64!(9)
        );
    }

    #[test]
    fn a_shared_target_never_falls_below_what_a_rendition_needs() {
        assert!(PlaylistContract::derive(&config(630_000, None), nz::u64!(2)).is_none());
    }

    #[test]
    fn a_presentation_without_renditions_has_no_target_duration() {
        assert_eq!(
            PlaylistContract::presentation_target_duration(std::iter::empty()),
            None
        );
    }
}
