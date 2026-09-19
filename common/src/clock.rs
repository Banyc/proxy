//! Injectable monotonic-clock seam for time-based decisions.
//!
//! Decisions that compare a stored instant against "now" (TTL expiry,
//! retention deadlines, the reverse-tunnel reconnect stability window) read
//! the current instant through [`Clock`] instead of calling
//! [`std::time::Instant::now`] at the decision site. Production uses
//! [`SystemClock`], which reads the real monotonic clock exactly as the call
//! sites did before; a test can install a controlled clock so the boundary
//! (which is otherwise reachable only by waiting real seconds, or never at
//! all for an exact-equality comparison) becomes deterministic.

use std::time::Instant;

/// A source of monotonic time.
///
/// The only production implementation is [`SystemClock`]. Test doubles return
/// a controlled instant so a boundary at exactly some deadline is drivable.
pub trait Clock: std::fmt::Debug + Send + Sync + 'static {
    /// The current instant. Monotonic and non-decreasing.
    fn now(&self) -> Instant;
}

/// The production [`Clock`]: the real monotonic clock.
#[derive(Debug, Clone, Copy, Default)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> Instant {
        Instant::now()
    }
}

#[cfg(test)]
pub(crate) mod test_support {
    use super::*;
    use std::{
        sync::atomic::{AtomicU64, Ordering},
        time::Duration,
    };

    /// A monotonic clock advanced only by explicit [`ManualClock::advance`]
    /// calls. Independent of wall time, so exact-instant boundaries are
    /// reachable.
    #[derive(Debug)]
    pub(crate) struct ManualClock {
        base: Instant,
        nanos: AtomicU64,
    }
    impl ManualClock {
        pub(crate) fn new() -> Self {
            Self {
                base: Instant::now(),
                nanos: AtomicU64::new(0),
            }
        }
        pub(crate) fn advance(&self, by: Duration) {
            self.nanos
                .fetch_add(by.as_nanos() as u64, Ordering::Relaxed);
        }
    }
    impl Clock for ManualClock {
        fn now(&self) -> Instant {
            self.base + Duration::from_nanos(self.nanos.load(Ordering::Relaxed))
        }
    }

    /// A clock that follows tokio's (possibly paused) virtual time, so a task
    /// driven under `#[tokio::test(start_paused = true)]` observes the same
    /// advance that `tokio::time::sleep` produces. Lets a deadline be placed
    /// exactly at virtual "now".
    #[derive(Debug)]
    pub(crate) struct VirtualClock {
        base: Instant,
        epoch: tokio::time::Instant,
    }
    impl VirtualClock {
        pub(crate) fn new() -> Self {
            Self {
                base: Instant::now(),
                epoch: tokio::time::Instant::now(),
            }
        }
    }
    impl Clock for VirtualClock {
        fn now(&self) -> Instant {
            self.base + self.epoch.elapsed()
        }
    }
}
