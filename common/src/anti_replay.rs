use std::{
    sync::Mutex,
    time::{Duration, Instant},
};

use primitive::{map::expiring_map::ExpiringHashMap, ops::len::Len};

pub const VALIDATOR_TIME_FRAME: Duration = Duration::from_secs(5);
pub const VALIDATOR_CAPACITY: usize = 1 << 16;

#[derive(Debug)]
pub struct ReplayValidator {
    time_frame: Duration,
    nonce: Mutex<NonceValidator>,
}
impl ReplayValidator {
    pub fn new(time_frame: Duration, capacity: usize) -> Self {
        Self {
            time_frame,
            nonce: Mutex::new(NonceValidator::new(time_frame, capacity)),
        }
    }
    pub fn time_validates(&self, timestamp: Duration) -> bool {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap();
        now.abs_diff(timestamp) < self.time_frame
    }
    pub fn nonce_validates(&self, nonce: [u8; tokio_chacha20::NONCE_BYTES]) -> bool {
        self.nonce.lock().unwrap().validates(nonce)
    }
}

#[derive(Debug)]
pub struct NonceValidator {
    seen: ExpiringHashMap<[u8; tokio_chacha20::NONCE_BYTES], (), Instant, Duration>,
    capacity: usize,
}
impl NonceValidator {
    pub fn new(time_frame: Duration, capacity: usize) -> Self {
        Self {
            seen: ExpiringHashMap::new(time_frame),
            capacity,
        }
    }
    pub fn validates(&mut self, nonce: [u8; tokio_chacha20::NONCE_BYTES]) -> bool {
        let now = Instant::now();
        self.seen.cleanup(now, |_, _, _| {});
        if self.seen.len() == self.capacity {
            return false;
        }
        if self.seen.insert(nonce, (), now).is_some() {
            return false;
        }
        true
    }
}
