use std::{
    borrow::Cow,
    fmt,
    net::SocketAddr,
    time::{SystemTime, UNIX_EPOCH},
};

use crate::{addr::InternetAddr, session_table::SessionTable};

pub type UdpSessionTable = SessionTable<Session>;

#[derive(Debug, Clone)]
pub struct Session {
    pub start: SystemTime,
    pub end: Option<SystemTime>,
    pub destination: InternetAddr,
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
        )?;
        if let Some(end) = self.end {
            write!(f, ", {}", end.duration_since(UNIX_EPOCH).unwrap().as_secs())?;
        }
        Ok(())
    }
}
