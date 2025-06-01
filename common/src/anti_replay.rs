use std::time::Duration;

pub const VALIDATOR_UDP_HDR_TTL: Duration = Duration::from_secs(60);
pub const VALIDATOR_TIME_FRAME: Duration = Duration::from_secs(5);
pub const VALIDATOR_CAPACITY: usize = 1 << 16;
