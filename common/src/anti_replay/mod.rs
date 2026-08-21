//! Anti-replay tuning constants for the proxy's header validators.
//!
//! The actual validator implementations live in the `ae` crate; this module
//! only pins the proxy's window sizes and capacities so the stream and UDP
//! validators (which use different windows) stay consistent. The UDP validator
//! window is `VALIDATOR_TIME_FRAME + VALIDATOR_UDP_HDR_TTL`.

use std::time::Duration;

pub const VALIDATOR_UDP_HDR_TTL: Duration = Duration::from_secs(60);
pub const VALIDATOR_TIME_FRAME: Duration = Duration::from_secs(5);
pub const VALIDATOR_CAPACITY: usize = 1 << 16;
