use std::{num::NonZeroU32, time::Duration};

use tokio::time::Instant;

use crate::domain::duration_from_nanos_saturating;

/// Live playlists must carry at least three target durations
/// (draft-pantos-hls-rfc8216bis-22, section 6.2.1), so this is a floor on the
/// window rather than a tunable: below it a conforming player cannot play.
pub const MINIMUM_PLAYLIST_SEGMENTS: usize = 3;
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
    /// How long media stays fetchable, and therefore how much the live
    /// playlist advertises.
    ///
    /// One quantity, not two. Media a playlist does not name is media no
    /// player can request, so a retention window wider than the advertised one
    /// would hold bytes nothing could reach. Both meanings move together.
    pub retain: DurationRule,
    pub part_tag_retention: DurationRule,
    /// How long a part URI remains fetchable after its tag disappears.
    pub part_fetch_grace_period: DurationRule,
    /// Shared retained media-and-manifest budget; media has priority.
    pub maximum_payload_bytes: usize,
    pub maximum_parts: usize,
    pub maximum_segments: usize,
}

impl RetentionPolicy {
    /// The advertised window, never below the protocol's three-segment floor.
    ///
    /// Raised rather than refused: an operator who wrote a short `retain` said
    /// something unambiguous, and refusing to serve over an arithmetic
    /// relationship they did not know about helps nobody. The configuration
    /// layer warns when it has to do this.
    pub fn minimum_playlist_duration_for(self, target: Duration) -> Duration {
        self.retain
            .resolve(target)
            .max(target.saturating_mul(u32::try_from(MINIMUM_PLAYLIST_SEGMENTS).unwrap_or(3)))
    }

    pub fn part_tag_retention_for(self, target: Duration) -> Duration {
        self.part_tag_retention.resolve(target)
    }

    pub fn part_fetch_deadline(self, removed_at: Instant, target: Duration) -> Option<Instant> {
        removed_at.checked_add(self.part_fetch_grace_period.resolve(target))
    }

    /// When a segment stops being fetchable, measured from publication.
    ///
    /// `retain` is a promise to viewers about how far back a playlist can
    /// point, so it is the only thing that decides this. The previous
    /// derivation — publication plus one segment plus the longest playlist the
    /// segment appeared in — made a long `retain` unreachable in practice,
    /// because a segment expired a playlist-window after publication however
    /// much retention had been configured.
    ///
    /// The advertised window is `retain` wide, so a segment reaches this
    /// deadline at about the moment it leaves the playlist. A client that
    /// started fetching it just before would otherwise lose it mid-transfer,
    /// so departure carries a grace of one target duration. That grace is
    /// derived from cadence rather than configured: it covers a request
    /// already in flight and nothing longer.
    pub fn segment_fetch_deadline(
        self,
        first_published_at: Instant,
        removed_at: Instant,
        target: Duration,
    ) -> Option<Instant> {
        let promised = first_published_at.checked_add(self.retain.resolve(target))?;
        let in_flight = removed_at.checked_add(target)?;
        Some(promised.max(in_flight))
    }
}

/// What one stream currently holds against the retention it asked for.
///
/// [`RetentionPolicy::retain`] is a request. What a playlist actually names
/// depends on how much media has been published and on the memory (and later
/// disk) cap. Reported so an operator who asked for two hours and received
/// seven minutes can see that, rather than reconstruct it from bitrate.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RetentionDepth {
    /// Configured `retain`.
    pub requested: Duration,
    /// Advertised playlist duration: visible completed parents plus any open
    /// parts. The longest rendition wins, so a shorter sibling does not hide
    /// how far back video still goes.
    pub held: Duration,
    /// Combined retained media and manifest bytes in memory, including
    /// fetch-grace media the playlist no longer names.
    pub memory_bytes: usize,
    /// Media buffers held by the store, excluding manifest caches.
    pub media_bytes: usize,
    /// Plain and gzip manifest bytes retained for reuse.
    pub manifest_bytes: usize,
    /// [`RetentionPolicy::maximum_payload_bytes`].
    pub memory_capacity: usize,
    /// Payload bytes in the disk overflow tier.
    pub disk_bytes: usize,
    /// Configured `disk.per_stream`, or zero when the node is memory-only.
    pub disk_capacity: usize,
}

impl RetentionDepth {
    /// Occupied retention tiers, newest media first.
    pub const fn tiers(self) -> [RetentionTier; 2] {
        [
            RetentionTier {
                name: "memory",
                bytes: self.memory_bytes,
                capacity: self.memory_capacity,
            },
            RetentionTier {
                name: "disk",
                bytes: self.disk_bytes,
                capacity: self.disk_capacity,
            },
        ]
    }
}

/// One retention tier's spend and cap for a stream.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RetentionTier {
    pub name: &'static str,
    pub bytes: usize,
    pub capacity: usize,
}

impl Default for RetentionPolicy {
    fn default() -> Self {
        Self {
            retain: Duration::from_mins(1).into(),
            part_tag_retention: TargetDurationMultiple::integer(3).into(),
            part_fetch_grace_period: TargetDurationMultiple::integer(3).into(),
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
        // `retain` is the whole answer: a segment stays fetchable for exactly
        // as long as the operator promised, however long the playlist it
        // appeared in happened to be.
        let published_at = Instant::now();
        let policy = RetentionPolicy {
            retain: Duration::from_hours(2).into(),
            ..RetentionPolicy::default()
        };

        let deadline = policy
            .segment_fetch_deadline(published_at, published_at, Duration::from_secs(6))
            .expect("the deadline fits");

        assert_eq!(
            deadline.duration_since(published_at),
            Duration::from_hours(2)
        );
    }

    #[test]
    fn the_advertised_window_never_falls_below_the_protocol_floor() {
        let policy = RetentionPolicy {
            retain: Duration::from_secs(5).into(),
            ..RetentionPolicy::default()
        };

        assert_eq!(
            policy.minimum_playlist_duration_for(Duration::from_secs(6)),
            Duration::from_secs(18),
            "a retain below three target durations is raised to it rather \
             than refused"
        );
    }
}
