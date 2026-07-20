#![cfg_attr(feature = "nightly", feature(test))]
#[cfg(feature = "nightly")]
extern crate test;

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
pub mod filter;
pub mod header;
pub mod loading;
pub mod log;
pub mod metrics;
pub mod notify;
pub mod proto;
pub mod route;
pub mod sampling;
pub mod stream;
pub mod suspend;
pub mod ttl_cell;
pub mod udp;
pub mod xor;
