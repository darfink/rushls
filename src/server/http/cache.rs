//! Reuse policy for shared immutable media responses.
//!
//! Manifest adapters decide their own cache semantics. Media paths are shared
//! across HLS and future protocols, so their HTTP lifetime belongs to the
//! application boundary that serves those paths rather than to one adapter or
//! the transport-neutral origin.

use std::time::Duration;

use crate::delivery::{
    Reuse,
    store::{DurationRule, TargetDurationMultiple},
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MediaCachePolicy {
    pub lifetime: DurationRule,
}

impl Default for MediaCachePolicy {
    fn default() -> Self {
        Self {
            lifetime: TargetDurationMultiple::new(6, nz::u32!(1)).into(),
        }
    }
}

impl MediaCachePolicy {
    pub fn reuse(self, target_duration: Duration) -> Reuse {
        Reuse::immutable(self.lifetime.resolve(target_duration))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn media_is_immutable_only_for_the_recommended_window() {
        let reuse = MediaCachePolicy::default().reuse(Duration::from_secs(6));

        assert!(reuse.immutable);
        assert_eq!(
            reuse.max_age,
            Duration::from_secs(36),
            "a republished stream reissues the identities this URL is built \
             from, so the promise has to expire"
        );
    }

    #[test]
    fn a_flat_lifetime_ignores_the_target_it_would_have_scaled() {
        let policy = MediaCachePolicy {
            lifetime: Duration::from_secs(31_536_000).into(),
        };

        assert_eq!(
            policy.reuse(Duration::from_secs(6)).max_age,
            Duration::from_secs(31_536_000)
        );
    }
}
