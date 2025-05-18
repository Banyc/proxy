use std::time::Duration;

use thiserror::Error;
use tracing::error;

pub mod respond;
pub mod server;

pub const PACKET_BUFFER_LENGTH: usize = 2_usize.pow(16);
pub const TIMEOUT: Duration = Duration::from_secs(10);

pub struct Packet {
    buf: udp_listener::Packet,
    pos: usize,
}

impl Packet {
    pub fn new(buf: udp_listener::Packet) -> Self {
        Self { buf, pos: 0 }
    }

    pub fn advance(&mut self, bytes: usize) -> Result<(), PacketPositionAdvancesOutOfRange> {
        let new_pos = bytes + self.pos;
        if new_pos > self.buf.len() {
            return Err(PacketPositionAdvancesOutOfRange);
        }
        self.pos = new_pos;
        Ok(())
    }

    pub fn slice(&self) -> &[u8] {
        &self.buf[self.pos..]
    }

    pub fn slice_mut(&mut self) -> &mut [u8] {
        &mut self.buf[self.pos..]
    }
}

#[derive(Debug, Error)]
#[error("Packet position advances out of range")]
pub struct PacketPositionAdvancesOutOfRange;
