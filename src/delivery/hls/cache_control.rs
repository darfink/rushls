//! How long a response may be reused by the caches between origin and viewer.
//!
//! Named for the header it becomes. Its neighbour [`cache`](super::cache) is
//! the origin's *own* reuse of rendered playlist bytes and holds no policy;
//! this module holds only policy and no bytes.
//!
//! Every lifetime is expressed against a target duration, because that is the
//! unit the protocol paces everything else by and the only figure that stays
//! correct across cadences. The defaults are the recommendations in
//! draft-pantos-hls-rfc8216bis-22 § Appendix B:
//!
//! | Response                          | Lifetime           |
//! |-----------------------------------|--------------------|
//! | Blocking playlist reload          | 6 target durations |
//! | Plain playlist request            | ½ target duration  |
//! | Media segment                     | 6 target durations |
//! | Absent, named by a directive      | 4 target durations |
//! | Absent otherwise                  | 1 target duration  |
//!
//! A response to a blocking reload is cacheable so much longer than a plain one
//! because the directive is part of the URL: `_HLS_msn=7` names one exact
//! playlist state, so the bytes answering it can never become wrong, while a
//! bare playlist URL means "the live edge" and is stale as it is written.

use std::time::Duration;

use crate::delivery::hls::{DurationRule, TargetDurationMultiple};

/// All response-lifetime policy for HLS delivery.
///
/// Consolidated so the relationships between the lifetimes stay visible: a
/// deployment lengthening its blocking-reload lifetime past what its clients
/// tolerate, or caching absence longer than it caches presence, should be able
/// to see that in one place rather than in five call sites.
///
/// Each lifetime is a [`DurationRule`], so a deployment that knows something
/// the protocol does not — a CDN with its own invalidation, or media URLs that
/// are unique for all time — can pin a flat duration instead of a multiple.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CacheControlPolicy {
    /// A playlist answering a request that named an exact position.
    pub blocking_playlist: DurationRule,
    /// A playlist answering a request for the live edge.
    pub playlist: DurationRule,
    /// Media named by a durable identity.
    ///
    /// Deliberately *not* infinite, though the bytes behind one of these URIs
    /// never change while the stream that produced them lives. A stream retired
    /// for idleness and then republished under the same name starts issuing
    /// segment and part identities from zero again, so a year-long lifetime
    /// would let a cache answer the new stream with the old stream's media. Six
    /// target durations is both what the specification recommends and shorter
    /// than any retirement worth configuring.
    pub media: DurationRule,
    /// A resource that does not exist, named by a request carrying a directive.
    pub blocking_missing: DurationRule,
    /// A resource that does not exist, or no longer does.
    pub missing: DurationRule,
    /// The target duration to scale by when the request resolved to no
    /// rendition — an unroutable path, or a stream this origin never had.
    pub assumed_target_duration: Duration,
}

impl Default for CacheControlPolicy {
    fn default() -> Self {
        Self {
            blocking_playlist: TargetDurationMultiple::new(6, nz::u32!(1)).into(),
            playlist: TargetDurationMultiple::new(1, nz::u32!(2)).into(),
            media: TargetDurationMultiple::new(6, nz::u32!(1)).into(),
            blocking_missing: TargetDurationMultiple::new(4, nz::u32!(1)).into(),
            missing: TargetDurationMultiple::new(1, nz::u32!(1)).into(),
            // The specification's recommended target duration, and the closest
            // thing to a right answer when the request named nothing that has
            // one of its own.
            assumed_target_duration: Duration::from_secs(6),
        }
    }
}

impl CacheControlPolicy {
    /// How long a projected playlist may be reused.
    pub fn playlist(&self, target: Option<Duration>, blocking: bool) -> CacheControl {
        let rule = if blocking {
            self.blocking_playlist
        } else {
            self.playlist
        };
        CacheControl::reusable(rule.resolve(self.target(target)))
    }

    /// How long media named by a durable identity may be reused.
    ///
    /// Marked immutable: within the lifetime it is granted, these bytes cannot
    /// change under their URL, so revalidating is pure round trip. That is a
    /// claim about the window, not about eternity, which is what makes it safe
    /// to pair with a bounded lifetime.
    pub fn media(&self, target: Option<Duration>) -> CacheControl {
        CacheControl {
            immutable: true,
            ..CacheControl::reusable(self.media.resolve(self.target(target)))
        }
    }

    /// How long the absence of a resource may be remembered.
    pub fn missing(&self, target: Option<Duration>, blocking: bool) -> CacheControl {
        let rule = if blocking {
            self.blocking_missing
        } else {
            self.missing
        };
        CacheControl::reusable(rule.resolve(self.target(target)))
    }

    fn target(&self, target: Option<Duration>) -> Duration {
        target.unwrap_or(self.assumed_target_duration)
    }
}

/// How long one response may be reused, resolved against a target duration.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct CacheControl {
    /// Zero asks for revalidation before every reuse.
    pub max_age: Duration,
    /// Whether the bytes are guaranteed not to change under this URL for as
    /// long as [`Self::max_age`] lasts.
    pub immutable: bool,
}

impl CacheControl {
    /// Reusable only after revalidating with the origin.
    ///
    /// The answer for anything momentary: a copy held for the round trip is
    /// still useful, but nobody may serve it on their own authority.
    pub const fn revalidate() -> Self {
        Self {
            max_age: Duration::ZERO,
            immutable: false,
        }
    }

    const fn reusable(max_age: Duration) -> Self {
        Self {
            max_age,
            immutable: false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TARGET: Option<Duration> = Some(Duration::from_secs(6));

    #[test]
    fn the_defaults_are_the_recommended_multiples_of_the_target_duration() {
        let policy = CacheControlPolicy::default();

        assert_eq!(
            policy.playlist(TARGET, true).max_age,
            Duration::from_secs(36)
        );
        assert_eq!(
            policy.playlist(TARGET, false).max_age,
            Duration::from_secs(3)
        );
        assert_eq!(policy.media(TARGET).max_age, Duration::from_secs(36));
        assert_eq!(
            policy.missing(TARGET, true).max_age,
            Duration::from_secs(24)
        );
        assert_eq!(
            policy.missing(TARGET, false).max_age,
            Duration::from_secs(6)
        );
        assert!(!policy.playlist(TARGET, true).immutable);
    }

    #[test]
    fn media_is_immutable_only_for_the_window_it_is_granted() {
        let media = CacheControlPolicy::default().media(TARGET);

        assert!(media.immutable);
        assert_eq!(
            media.max_age,
            Duration::from_secs(36),
            "a republished stream reissues the identities this URL is built \
             from, so the promise has to expire"
        );
    }

    #[test]
    fn a_flat_lifetime_ignores_the_target_it_would_have_scaled() {
        let policy = CacheControlPolicy {
            media: Duration::from_secs(31_536_000).into(),
            ..CacheControlPolicy::default()
        };

        assert_eq!(
            policy.media(TARGET).max_age,
            Duration::from_secs(31_536_000)
        );
    }

    #[test]
    fn an_unresolved_request_falls_back_to_the_assumed_target() {
        let policy = CacheControlPolicy {
            assumed_target_duration: Duration::from_secs(4),
            ..CacheControlPolicy::default()
        };

        assert_eq!(policy.missing(None, false).max_age, Duration::from_secs(4));
        assert_eq!(policy.playlist(None, false).max_age, Duration::from_secs(2));
    }

    #[test]
    fn a_lifetime_below_a_second_asks_for_revalidation_rather_than_rounding_up() {
        // Half of a one-second target is not representable in a header that
        // carries whole seconds, and erring long would serve a live edge that
        // has already moved.
        let caching = CacheControlPolicy::default().playlist(Some(Duration::from_secs(1)), false);

        assert_eq!(caching.max_age.as_secs(), 0);
        assert_eq!(CacheControl::revalidate().max_age.as_secs(), 0);
    }
}
