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

use std::{
    io,
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
}

impl Default for AcceptErrors {
    fn default() -> Self {
        Self {
            backoff: INITIAL_BACKOFF,
            unreported: 0,
            last_reported: None,
        }
    }
}

impl AcceptErrors {
    /// Records a failure and says whether, and when, to accept again.
    pub fn failed(&mut self, error: &io::Error) -> Next {
        self.failed_at(error, Instant::now())
    }

    fn failed_at(&mut self, error: &io::Error, now: Instant) -> Next {
        if is_fatal(error) {
            return Next::Stop;
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
        *self = Self::default();
        recovered
    }
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
