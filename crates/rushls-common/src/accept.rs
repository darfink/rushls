//! What an accept loop does when `accept` fails.
//!
//! Almost every failure is temporary, and nothing about the listener is
//! wrong: the process is out of descriptors or buffers (`EMFILE`, `ENFILE`,
//! `ENOBUFS`, `ENOMEM`), or a peer went away between the kernel completing its
//! handshake and the call returning it (`ECONNABORTED`, and on Linux the
//! network errors `accept(2)` says to treat like `EAGAIN`). Stopping on those
//! would turn a connection burst into dropping every live session, so the loop
//! retries, backing off so an exhausted descriptor table does not spin a core.
//!
//! A few errors mean the listening socket itself is unusable (`EBADF`,
//! `EINVAL`, `ENOTSOCK`, `EFAULT`). Retrying those would loop forever without
//! ever accepting again, so they are fatal: the caller stops accepting, and the
//! orchestrator restarts a working process.
//!
//! Nothing here logs. A persistent failure would otherwise produce one line per
//! attempt, so [`AcceptErrors`] says when a summary is due and the application
//! reports it through its own events or logs.
//!
//! A temporary failure that never clears, such as a descriptor table that
//! stays full, would otherwise leave a process that refuses every connection
//! while its probes report it healthy. [`AcceptHealth`] turns that into a
//! readiness failure, so a load balancer stops sending new connections. It
//! never fails liveness: a restart would cut every live session, and the usual
//! cure is those sessions ending and freeing descriptors.

use std::{
    io,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

/// First pause after a failure. Short, because the usual cause is one aborted
/// connection and the next `accept` succeeds.
pub const INITIAL_BACKOFF: Duration = Duration::from_millis(50);
/// Longest pause. Bounds how long a recovered listener can sit idle, and
/// keeps a persistent failure at one wakeup per second.
pub const MAXIMUM_BACKOFF: Duration = Duration::from_secs(1);
/// How often a persistent failure is reported after its first occurrence.
pub const REPORT_INTERVAL: Duration = Duration::from_secs(10);

/// How long `accept` must keep failing before [`AcceptHealth`] reports it.
///
/// Long enough that a burst of aborted connections or a brief descriptor spike
/// never takes a replica out of rotation; short enough that one which cannot
/// accept anyone stops being sent connections.
pub const PERSISTENT_FAILURE: Duration = Duration::from_secs(30);

/// Without a failure for this long, a run of failures has ended.
///
/// Needed because recovery is not always observable as a success: once
/// readiness fails, the load balancer stops sending connections, so no
/// `accept` may succeed for a long time. A quiet listener is a recovered one.
/// The backoff tops out at [`MAXIMUM_BACKOFF`], so a continuing failure is
/// never this quiet.
pub const QUIET_AFTER_FAILURE: Duration = Duration::from_secs(10);

/// Whether an `accept` error leaves the listener unusable.
pub fn is_fatal(error: &io::Error) -> bool {
    #[cfg(unix)]
    {
        matches!(
            error.raw_os_error(),
            Some(libc::EBADF | libc::EINVAL | libc::ENOTSOCK | libc::EFAULT)
        )
    }
    #[cfg(not(unix))]
    {
        // Without errno, only the portable spelling of "this is not a usable
        // listener" is recognised; everything else is retried.
        error.kind() == io::ErrorKind::InvalidInput
    }
}

/// What to do after one failed `accept`.
#[derive(Debug, Eq, PartialEq)]
pub enum Next {
    /// Accept again after `pause`.
    Retry {
        pause: Duration,
        /// Failures to report now, this one included, or `None` while the
        /// previous report is recent. Reported failures are not counted twice.
        report: Option<u64>,
    },
    /// The listener cannot recover; stop accepting.
    Stop,
}

/// Failure bookkeeping for one listener's accept loop.
#[derive(Debug)]
pub struct AcceptErrors {
    backoff: Duration,
    /// Failures since the last report, included in the next one.
    unreported: u64,
    last_reported: Option<Instant>,
    /// Shared with a readiness probe, when the listener has one.
    health: Option<AcceptHealth>,
}

impl Default for AcceptErrors {
    fn default() -> Self {
        Self {
            backoff: INITIAL_BACKOFF,
            unreported: 0,
            last_reported: None,
            health: None,
        }
    }
}

impl AcceptErrors {
    /// Also records failures and successes in `health`, for readiness.
    pub fn with_health(health: AcceptHealth) -> Self {
        Self {
            health: Some(health),
            ..Self::default()
        }
    }

    /// Records a failure and says whether, and when, to accept again.
    pub fn failed(&mut self, error: &io::Error) -> Next {
        self.failed_at(error, Instant::now())
    }

    fn failed_at(&mut self, error: &io::Error, now: Instant) -> Next {
        if is_fatal(error) {
            return Next::Stop;
        }
        if let Some(health) = &self.health {
            health.failed_at(now);
        }
        self.unreported += 1;
        let due = self
            .last_reported
            .is_none_or(|last| now.duration_since(last) >= REPORT_INTERVAL);
        let report = due.then(|| {
            self.last_reported = Some(now);
            std::mem::take(&mut self.unreported)
        });
        let pause = self.backoff;
        self.backoff = (self.backoff * 2).min(MAXIMUM_BACKOFF);
        Next::Retry { pause, report }
    }

    /// Records a successful `accept`, so the next failure starts afresh.
    ///
    /// Returns whether failures had been reported since the last success, for
    /// a caller that announces recovery.
    pub fn succeeded(&mut self) -> bool {
        let recovered = self.last_reported.is_some();
        if let Some(health) = &self.health {
            health.succeeded();
        }
        self.backoff = INITIAL_BACKOFF;
        self.unreported = 0;
        self.last_reported = None;
        recovered
    }
}

/// Whether one listener has been failing to accept for long enough that its
/// process should stop receiving new connections.
///
/// Cheap to clone and lock-free, because a readiness probe reads it while the
/// accept loop writes it. Feed it through [`AcceptErrors::with_health`].
#[derive(Clone, Debug)]
pub struct AcceptHealth(Arc<HealthState>);

#[derive(Debug)]
struct HealthState {
    /// The clock the two instants below are measured on.
    origin: Instant,
    /// Milliseconds since `origin`, plus one, when the current run of
    /// failures began; zero when there is none.
    failing_since: AtomicU64,
    /// Milliseconds since `origin`, plus one, of the latest failure.
    failed_at: AtomicU64,
}

impl Default for AcceptHealth {
    fn default() -> Self {
        Self(Arc::new(HealthState {
            origin: Instant::now(),
            failing_since: AtomicU64::new(0),
            failed_at: AtomicU64::new(0),
        }))
    }
}

impl AcceptHealth {
    /// Whether `accept` has failed continuously for [`PERSISTENT_FAILURE`],
    /// most recently within [`QUIET_AFTER_FAILURE`].
    pub fn is_failing(&self) -> bool {
        self.is_failing_at(Instant::now())
    }

    fn is_failing_at(&self, now: Instant) -> bool {
        let since = self.0.failing_since.load(Ordering::Relaxed);
        let last = self.0.failed_at.load(Ordering::Relaxed);
        if since == 0 || last == 0 {
            return false;
        }
        self.millis(now).saturating_sub(last) <= millis(QUIET_AFTER_FAILURE)
            && last.saturating_sub(since) >= millis(PERSISTENT_FAILURE)
    }

    fn failed_at(&self, now: Instant) {
        let now = self.millis(now);
        let last = self.0.failed_at.swap(now, Ordering::Relaxed);
        // A failure after a quiet spell starts a new run rather than
        // extending one that has already ended.
        if last == 0 || now.saturating_sub(last) > millis(QUIET_AFTER_FAILURE) {
            self.0.failing_since.store(now, Ordering::Relaxed);
        }
    }

    fn succeeded(&self) {
        self.0.failing_since.store(0, Ordering::Relaxed);
        self.0.failed_at.store(0, Ordering::Relaxed);
    }

    /// Plus one, so zero can mean "never".
    fn millis(&self, now: Instant) -> u64 {
        millis(now.saturating_duration_since(self.0.origin)) + 1
    }
}

fn millis(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    fn os(code: i32) -> io::Error {
        io::Error::from_raw_os_error(code)
    }

    #[test]
    fn only_errors_about_the_listener_itself_are_fatal() {
        for code in [libc::EBADF, libc::EINVAL, libc::ENOTSOCK, libc::EFAULT] {
            assert!(is_fatal(&os(code)), "{code}");
        }
        for code in [
            libc::EMFILE,
            libc::ENFILE,
            libc::ENOBUFS,
            libc::ENOMEM,
            libc::ECONNABORTED,
            libc::EINTR,
            libc::EPROTO,
            libc::EPERM,
            libc::ENETDOWN,
            libc::EHOSTUNREACH,
            libc::EOPNOTSUPP,
        ] {
            assert!(!is_fatal(&os(code)), "{code}");
        }
    }

    #[test]
    fn backoff_doubles_to_a_ceiling_and_resets_on_success() {
        let mut errors = AcceptErrors::default();
        let now = Instant::now();
        let pauses: Vec<_> = (0..8)
            .map(|_| match errors.failed_at(&os(libc::EMFILE), now) {
                Next::Retry { pause, .. } => pause,
                Next::Stop => panic!("EMFILE is temporary"),
            })
            .collect();
        assert_eq!(pauses[0], INITIAL_BACKOFF);
        assert_eq!(pauses[1], INITIAL_BACKOFF * 2);
        assert_eq!(pauses[7], MAXIMUM_BACKOFF);

        assert!(errors.succeeded(), "the first failure was reported");
        assert!(matches!(
            errors.failed_at(&os(libc::EMFILE), now),
            Next::Retry {
                pause: INITIAL_BACKOFF,
                report: Some(1)
            }
        ));
    }

    #[test]
    fn readiness_fails_only_after_persistent_failure_and_recovers() {
        let health = AcceptHealth::default();
        let mut errors = AcceptErrors::with_health(health.clone());
        let start = Instant::now();
        let at = |seconds| start + Duration::from_secs(seconds);

        // One failure a second, as the capped backoff produces.
        for second in 0..30 {
            errors.failed_at(&os(libc::EMFILE), at(second));
        }
        assert!(
            !health.is_failing_at(at(29)),
            "29 seconds of failure is still a burst"
        );
        errors.failed_at(&os(libc::EMFILE), at(30));
        assert!(health.is_failing_at(at(30)), "30 seconds is persistent");

        assert!(
            !health.is_failing_at(at(41)),
            "ten seconds without a failure ends the run"
        );
        errors.failed_at(&os(libc::EMFILE), at(42));
        assert!(
            !health.is_failing_at(at(42)),
            "a failure after a quiet spell starts a new run"
        );

        for second in 43..=72 {
            errors.failed_at(&os(libc::EMFILE), at(second));
        }
        assert!(health.is_failing_at(at(72)));
        errors.succeeded();
        assert!(!health.is_failing_at(at(72)), "a success ends the run");
    }

    #[test]
    fn a_fatal_error_does_not_count_as_a_failing_run() {
        let health = AcceptHealth::default();
        let mut errors = AcceptErrors::with_health(health.clone());
        assert_eq!(errors.failed(&os(libc::EBADF)), Next::Stop);
        assert!(!health.is_failing());
    }

    #[test]
    fn a_fatal_error_stops_the_loop() {
        assert_eq!(AcceptErrors::default().failed(&os(libc::EBADF)), Next::Stop);
    }

    #[test]
    fn a_persistent_failure_is_reported_as_a_summary() {
        let mut errors = AcceptErrors::default();
        let start = Instant::now();
        let report = |next| match next {
            Next::Retry { report, .. } => report,
            Next::Stop => panic!("EMFILE is temporary"),
        };
        assert_eq!(
            report(errors.failed_at(&os(libc::EMFILE), start)),
            Some(1),
            "the first failure is reported at once"
        );
        for _ in 0..5 {
            let later = start + Duration::from_secs(1);
            assert_eq!(report(errors.failed_at(&os(libc::EMFILE), later)), None);
        }
        assert_eq!(
            report(errors.failed_at(&os(libc::EMFILE), start + REPORT_INTERVAL)),
            Some(6),
            "the summary counts the repeats inside the interval"
        );
    }
}
