use std::{
    fmt,
    net::SocketAddr,
    time::{SystemTime, UNIX_EPOCH},
};

use crate::session_table::SessionTable;

use super::addr::StreamAddr;

pub type StreamSessionTable = SessionTable<Session>;

#[derive(Debug, Clone)]
pub struct Session {
    pub start: SystemTime,
    pub destination: StreamAddr,
    pub upstream_local: SocketAddr,
}

impl fmt::Display for Session {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{}: {} -> {}",
            self.start.duration_since(UNIX_EPOCH).unwrap().as_secs(),
            self.upstream_local,
            self.destination
        )
    }
}
