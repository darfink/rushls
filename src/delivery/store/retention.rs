use std::{num::NonZeroU32, time::Duration};

use tokio::time::Instant;

use crate::domain::duration_from_nanos_saturating;

const DEFAULT_VISIBLE_SEGMENTS: usize = 6;
const DEFAULT_MAXIMUM_RETAINED_PAYLOAD_BYTES: usize = 512 * 1024 * 1024;
const DEFAULT_MAXIMUM_RETAINED_PARTS: usize = 16_384;
const DEFAULT_MAXIMUM_RETAINED_SEGMENTS: usize = 4_096;

/// An exact multiple of a rendition's target duration.
///
/// A rational representation keeps recommendations such as 1.5× exact in
/// configuration and avoids making policy equality depend on floating point.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TargetDurationMultiple {
    numerator: u32,
    denominator: NonZeroU32,
}

impl TargetDurationMultiple {
    pub const fn new(numerator: u32, denominator: NonZeroU32) -> Self {
        Self {
            numerator,
            denominator,
        }
    }

    pub const fn integer(value: u32) -> Self {
        Self {
            numerator: value,
            denominator: NonZeroU32::MIN,
        }
    }

    pub const fn numerator(self) -> u32 {
        self.numerator
    }

    pub const fn denominator(self) -> NonZeroU32 {
        self.denominator
    }

    /// Scales a target duration by this multiple, exactly.
    ///
    /// Public because playlist projection sizes hold-backs the same way
    /// retention sizes windows, and two implementations of one calculation is
    /// how a playlist ends up promising something retention does not keep.
    pub fn apply(self, target: Duration) -> Duration {
        // Round upward: a retention recommendation is a minimum, so losing a
        // fractional nanosecond must not make the configured window shorter.
        let nanos = target
            .as_nanos()
            .saturating_mul(u128::from(self.numerator))
            .div_ceil(u128::from(self.denominator.get()));
        duration_from_nanos_saturating(nanos)
    }
}

/// A duration independent of, or relative to, the target duration.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DurationRule {
    Fixed(Duration),
    MultipleOfTarget(TargetDurationMultiple),
}

impl DurationRule {
    /// Sizes this rule against the target duration it is relative to.
    ///
    /// Public because response freshness is expressed in the same terms as
    /// retention: a lifetime the protocol paces by target durations, or a flat
    /// one a deployment pins for its own reasons.
    pub fn resolve(self, target: Duration) -> Duration {
        match self {
            Self::Fixed(duration) => duration,
            Self::MultipleOfTarget(multiple) => multiple.apply(target),
        }
    }
}

impl From<Duration> for DurationRule {
    fn from(duration: Duration) -> Self {
        DurationRule::Fixed(duration)
    }
}

impl From<TargetDurationMultiple> for DurationRule {
    fn from(multiple: TargetDurationMultiple) -> Self {
        DurationRule::MultipleOfTarget(multiple)
    }
}

/// All playlist-visibility, resource-availability, and capacity policy for one
/// logical HLS stream.
///
/// Keeping these related rules together prevents playlist windows, standalone
/// resource grace periods, and safety bounds from silently diverging. The
/// default encodes the current HLS/Apple-aligned behavior, while exact
/// fractional target-duration multiples allow deployment-specific guidance.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RetentionPolicy {
    pub minimum_playlist_segments: usize,
    pub minimum_playlist_duration: DurationRule,
    pub part_tag_retention: DurationRule,
    /// How long a part URI remains fetchable after its tag disappears.
    pub part_fetch_grace_period: DurationRule,
    /// Optional segment grace beginning when its tag disappears.
    ///
    /// `None` preserves the protocol-derived availability deadline based on
    /// the longest playlist in which the segment appeared.
    pub segment_fetch_grace_period: Option<DurationRule>,
    pub maximum_payload_bytes: usize,
    pub maximum_parts: usize,
    pub maximum_segments: usize,
}

impl RetentionPolicy {
    pub fn minimum_playlist_duration_for(self, target: Duration) -> Duration {
        self.minimum_playlist_duration.resolve(target)
    }

    pub fn part_tag_retention_for(self, target: Duration) -> Duration {
        self.part_tag_retention.resolve(target)
    }

    pub fn part_fetch_deadline(self, removed_at: Instant, target: Duration) -> Option<Instant> {
        removed_at.checked_add(self.part_fetch_grace_period.resolve(target))
    }

    pub fn segment_fetch_deadline(
        self,
        removed_at: Instant,
        target: Duration,
        first_published_at: Instant,
        segment_duration: Duration,
        longest_playlist_duration: Duration,
    ) -> Option<Instant> {
        match self.segment_fetch_grace_period {
            Some(rule) => removed_at.checked_add(rule.resolve(target)),
            None => first_published_at
                .checked_add(segment_duration.saturating_add(longest_playlist_duration)),
        }
    }
}

impl Default for RetentionPolicy {
    fn default() -> Self {
        Self {
            minimum_playlist_segments: DEFAULT_VISIBLE_SEGMENTS,
            minimum_playlist_duration: TargetDurationMultiple::integer(3).into(),
            part_tag_retention: TargetDurationMultiple::integer(3).into(),
            part_fetch_grace_period: TargetDurationMultiple::integer(3).into(),
            segment_fetch_grace_period: None,
            maximum_payload_bytes: DEFAULT_MAXIMUM_RETAINED_PAYLOAD_BYTES,
            maximum_parts: DEFAULT_MAXIMUM_RETAINED_PARTS,
            maximum_segments: DEFAULT_MAXIMUM_RETAINED_SEGMENTS,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fractional_target_duration_rules_are_exact_and_round_up() {
        let one_and_a_half = TargetDurationMultiple::new(3, nz::u32!(2));
        let rule: DurationRule = one_and_a_half.into();

        assert_eq!(rule.resolve(Duration::from_secs(6)), Duration::from_secs(9));
        assert_eq!(
            rule.resolve(Duration::from_nanos(1)),
            Duration::from_nanos(2),
            "minimum retention rules round fractional nanoseconds upward"
        );
    }

    #[test]
    fn a_segment_fetch_grace_override_starts_when_the_tag_is_removed() {
        let removed_at = Instant::now();
        let policy = RetentionPolicy {
            segment_fetch_grace_period: Some(TargetDurationMultiple::new(3, nz::u32!(2)).into()),
            ..RetentionPolicy::default()
        };

        let deadline = policy
            .segment_fetch_deadline(
                removed_at,
                Duration::from_secs(6),
                removed_at,
                Duration::from_secs(6),
                Duration::from_secs(36),
            )
            .expect("the deadline fits");

        assert_eq!(deadline.duration_since(removed_at), Duration::from_secs(9));
    }
}
