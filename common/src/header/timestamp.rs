use core::time::Duration;

pub const TIME_FRAME: Duration = Duration::from_secs(5);

#[derive(Debug, Clone)]
pub struct TimestampMsg {
    pub timestamp_sec: u64,
}
impl TimestampMsg {
    pub fn now() -> Self {
        let timestamp_sec = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        Self { timestamp_sec }
    }
    pub fn timestamp(&self) -> Duration {
        Duration::from_secs(self.timestamp_sec)
    }
    pub const SIZE: usize = std::mem::size_of::<u64>();
    pub fn encode(&self) -> [u8; Self::SIZE] {
        self.timestamp_sec.to_be_bytes()
    }
    pub fn decode(buf: [u8; Self::SIZE]) -> Self {
        let timestamp_sec = u64::from_be_bytes(buf);
        Self { timestamp_sec }
    }
}

#[derive(Debug, Clone)]
pub struct ReplayValidator {
    time_frame: Duration,
}
impl ReplayValidator {
    pub fn new(time_frame: Duration) -> Self {
        Self { time_frame }
    }
    pub fn validates(&self, timestamp: Duration) -> bool {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap();
        now.abs_diff(timestamp) < self.time_frame
    }
}
