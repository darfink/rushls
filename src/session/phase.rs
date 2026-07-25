use derive_more::Display;

/// Where a session is in its lifecycle.
///
/// The order matches the order of steps in
/// [`run_session`](super::run_session); a phase is never set from anywhere but
/// the step it names.
#[derive(Clone, Copy, Debug, Display, Eq, Ord, PartialEq, PartialOrd)]
#[display(rename_all = "lowercase")]
pub enum Phase {
    /// Admitted, transport accepted, nothing read yet.
    Accepted,
    /// Probing the input for its track set.
    Discovering,
    /// Checking the discovered tracks against the publisher's policy.
    Validating,
    /// Placing tracks on a shared presentation origin. Needs no media.
    Calibrating,
    /// Observing media until segmentation cadence can be fixed.
    Segmenting,
    /// Muxing and publishing.
    Running,
    /// Input ended or the session was stopped; flushing what remains.
    Draining,
}

impl Phase {
    /// Whether media has yet to reach the muxer.
    ///
    /// Health treats these phases as waiting rather than stalled: there is no
    /// publication cadence to fall behind before segmentation locks.
    pub fn is_before_publication(self) -> bool {
        self < Self::Running
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_running_and_draining_expect_a_publication_cadence() {
        assert!(Phase::Accepted.is_before_publication());
        assert!(Phase::Segmenting.is_before_publication());
        assert!(!Phase::Running.is_before_publication());
        assert!(!Phase::Draining.is_before_publication());
    }
}
