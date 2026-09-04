#![cfg_attr(feature = "nightly", feature(test))]
#![warn(clippy::disallowed_methods, clippy::disallowed_types)]
#[cfg(feature = "nightly")]
extern crate test;

use std::{fmt, time::Duration};

/// Stream I/O timeout used across the relay plumbing (connect/header/copy).
pub const STREAM_IO_TIMEOUT: Duration = Duration::from_secs(60);

/// Wraps an `Option` for log rendering so a value prints on its own when
/// present — no `Some(...)` — and nothing at all when `None` — no `None`
/// item. Use with `?OptLog(x)`/`%OptLog(x)` in tracing fields or embed it
/// in a `Display` message.
pub struct OptLog<T>(pub Option<T>);
impl<T: fmt::Display> fmt::Display for OptLog<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if let Some(v) = &self.0 {
            write!(f, "{v}")?;
        }
        Ok(())
    }
}
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
