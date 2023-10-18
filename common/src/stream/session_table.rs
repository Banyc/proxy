use std::{
    borrow::Cow,
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
    pub upstream_local: Option<SocketAddr>,
}

impl fmt::Display for Session {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let upstream_local: Cow<str> = match self.upstream_local {
            Some(upstream_local) => upstream_local.to_string().into(),
            None => "?".into(),
        };
        write!(
            f,
            "{}: {} -> {}",
            self.start.duration_since(UNIX_EPOCH).unwrap().as_secs(),
            upstream_local,
            self.destination
        )
    }
}
