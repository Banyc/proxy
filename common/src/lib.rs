#![cfg_attr(feature = "nightly", feature(test))]
#![warn(clippy::disallowed_methods, clippy::disallowed_types)]
#[cfg(feature = "nightly")]
extern crate test;

use std::{fmt, time::Duration};

/// Stream I/O timeout used across the relay plumbing (connect/header/copy).
pub const STREAM_IO_TIMEOUT: Duration = Duration::from_secs(60);

/// Wraps an `Option` for log rendering so a value prints on its own when
/// present — no `Some(...)` — and nothing at all when `None` — no `None`
/// item. Use with `?OptLog(x)` in tracing fields.
pub struct OptLog<T>(pub Option<T>);
impl<T: fmt::Debug> fmt::Debug for OptLog<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if let Some(v) = &self.0 {
            write!(f, "{v:?}")?;
        }
        Ok(())
    }
}

pub mod addr;
pub mod anti_replay;
pub mod clock;
pub mod config;
pub mod connect;
pub mod error;
pub mod header;
pub mod lifecycle;
pub mod loading;
pub mod log;
pub mod matcher;
pub mod metrics;
pub mod notify;
pub mod proxy_runtime;
pub mod route;
pub mod session;
pub mod stream_runtime;
pub mod ttl_cell;
pub mod udp_runtime;

#[cfg(test)]
mod test_alloc;
#[cfg(test)]
mod test_metrics;
#[cfg(test)]
#[global_allocator]
static TEST_ALLOCATOR: test_alloc::CountingAllocator = test_alloc::CountingAllocator;

/// Guards the ingress path's deadlines against the operator's field numbers.
///
/// The operator's path measures a 190 ms minimum round trip with maxima of
/// 1063 ms and 3205 ms. A deadline that expires *inside* that range turns a
/// live-but-slow ingress into a failed one, and a reconnect during a spike is
/// worse than the spike: the rebirth it forces pays the cold establishment the
/// whole ingress path exists to avoid. Every field-reachable timer on the
/// birth path must therefore expire only on evidence of death, not on elapsed
/// slowness.
#[cfg(test)]
mod birth_path_deadlines {
    use super::STREAM_IO_TIMEOUT;
    use crate::anti_replay::VALIDATOR_TIME_FRAME;
    use std::time::Duration;

    /// The field's worst recorded round trip, one authority for its value:
    /// `rtp_mux/tests/birth_liveness.rs` (`FIELD_WORST_ROUND_TRIP`).
    const FIELD_WORST_ROUND_TRIP: Duration = Duration::from_millis(3205);

    /// `STREAM_IO_TIMEOUT` bounds the ingress header read, the first-hop
    /// connect and the relay copy's idle read/write, so it is the deadline
    /// that would fail a slow ingress. It must outlast the field's worst
    /// round trip.
    #[test]
    fn stream_io_timeout_outlasts_the_fields_worst_round_trip() {
        assert!(
            STREAM_IO_TIMEOUT > FIELD_WORST_ROUND_TRIP,
            "STREAM_IO_TIMEOUT = {STREAM_IO_TIMEOUT:?} must outlast the field's worst round trip \
             {FIELD_WORST_ROUND_TRIP:?}; below it the deadline fires on a spike, not on death"
        );
    }

    /// The relay header is stamped when the proxy client builds it and judged
    /// when the access server reads it, so its age at judgement is bounded by
    /// the path's worst round trip (conservatively — the header travels one
    /// way). The anti-replay time frame is a symmetric window
    /// (`ae::anti_replay::TimeValidator::validates`), and a header outside it
    /// is refused as stale; it must therefore exceed that bound.
    #[test]
    fn validator_time_frame_outlasts_the_fields_worst_round_trip() {
        assert!(
            VALIDATOR_TIME_FRAME > FIELD_WORST_ROUND_TRIP,
            "VALIDATOR_TIME_FRAME = {VALIDATOR_TIME_FRAME:?} must outlast the field's worst round \
             trip {FIELD_WORST_ROUND_TRIP:?}; below it a spike is refused as a stale header"
        );
    }
}
