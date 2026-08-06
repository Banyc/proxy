#![cfg_attr(feature = "nightly", feature(test))]
#![warn(clippy::disallowed_methods, clippy::disallowed_types)]
#[cfg(feature = "nightly")]
extern crate test;

use std::time::Duration;

/// Stream I/O timeout used across the relay plumbing (connect/header/copy).
pub const STREAM_IO_TIMEOUT: Duration = Duration::from_secs(60);

/// Print a line to stdout when the INFO log level is enabled.
/// Replaces noisy `info!()` for user-facing terminal output.
#[macro_export]
macro_rules! info_println {
    ($($arg:tt)*) => {
        if tracing::level_enabled!(tracing::Level::INFO) {
            println!($($arg)*);
        }
    }
}

pub mod addr;
pub mod anti_replay;
pub mod config;
pub mod connect;
pub mod error;
pub mod header;
pub mod loading;
pub mod log;
pub mod matcher;
pub mod metrics;
pub mod notify;
pub mod process;
pub mod proto;
pub mod retention;
pub mod route;
pub mod serve_loop;
pub mod session;
pub mod stream;
pub mod suspend;
pub mod ttl_cell;
pub mod udp;
