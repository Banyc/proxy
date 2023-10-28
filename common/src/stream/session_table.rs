use std::{
    borrow::Cow,
    net::SocketAddr,
    time::{SystemTime, UNIX_EPOCH},
};

use tabled::Tabled;

use crate::session_table::SessionTable;

use super::addr::StreamAddr;

pub type StreamSessionTable = SessionTable<Session>;

#[derive(Debug, Clone)]
pub struct Session {
    pub start: SystemTime,
    pub end: Option<SystemTime>,
    pub destination: StreamAddr,
    pub upstream_local: Option<SocketAddr>,
}

impl Tabled for Session {
    const LENGTH: usize = 4;

    fn fields(&self) -> Vec<Cow<'_, str>> {
        let start_unix = self.start.duration_since(UNIX_EPOCH).unwrap();
        let now_unix = SystemTime::now().duration_since(UNIX_EPOCH).unwrap();

        let start = start_unix.as_secs().to_string().into();
        let upstream_local = match self.upstream_local {
            Some(upstream_local) => upstream_local.to_string().into(),
            None => "".into(),
        };
        let destination = self.destination.to_string().into();
        let duration = match self.end {
            Some(end) => end
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .saturating_sub(start_unix),
            None => now_unix.saturating_sub(start_unix),
        };
        let duration = if duration.as_secs() == 0 {
            format!("{} ms", duration.as_millis())
        } else if duration.as_secs() / 60 == 0 {
            format!("{} s", duration.as_secs())
        } else if duration.as_secs() / 60 / 60 == 0 {
            format!("{} min", duration.as_secs() / 60)
        } else {
            format!("{} h", duration.as_secs() / 60 / 60)
        }
        .into();
        vec![destination, duration, start, upstream_local]
    }

    fn headers() -> Vec<Cow<'static, str>> {
        vec![
            "destination".into(),
            "duration".into(),
            "start".into(),
            "upstream_local".into(),
        ]
    }
}
