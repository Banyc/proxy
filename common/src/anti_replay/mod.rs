//! Anti-replay tuning constants for the proxy's header validators.
//!
//! The actual validator implementations live in the `ae` crate; this module
//! only pins the proxy's window sizes and capacities so the stream and UDP
//! validators (which use different windows) stay consistent.

use std::time::Duration;

pub const VALIDATOR_UDP_HDR_TTL: Duration = Duration::from_secs(60);
pub const VALIDATOR_TIME_FRAME: Duration = Duration::from_secs(5);
pub const VALIDATOR_CAPACITY: usize = 1 << 16;

/// The window every UDP-path validator judges a header timestamp against.
///
/// The UDP path keeps no nonce cache, so this window alone bounds how long a
/// captured header stays replayable. Both ends of a proxy connection judge the
/// timestamp independently and must derive the same window: one that accepts a
/// header the other refuses either drops a live request or serves a replay.
///
/// `saturating_add` rather than `+` only because `Add` is not usable in a
/// `const`; the operands are the two constants above, both whole seconds far
/// below `Duration::MAX`, so the value is `VALIDATOR_TIME_FRAME +
/// VALIDATOR_UDP_HDR_TTL` exactly.
pub const VALIDATOR_UDP_WINDOW: Duration =
    VALIDATOR_TIME_FRAME.saturating_add(VALIDATOR_UDP_HDR_TTL);

#[cfg(test)]
mod tests {
    use super::*;

    /// The composition is the contract, not either constant on its own: the
    /// window is the time frame plus the header TTL. Tuning a constant stays a
    /// one-line change; changing the pairing fails here.
    #[test]
    fn the_udp_window_is_the_time_frame_plus_the_header_ttl() {
        assert_eq!(
            VALIDATOR_UDP_WINDOW,
            VALIDATOR_TIME_FRAME + VALIDATOR_UDP_HDR_TTL
        );
    }
}
