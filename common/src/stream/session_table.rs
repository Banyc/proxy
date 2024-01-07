use std::{
    fmt,
    net::SocketAddr,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use monitor_table::{
    row::{LiteralType, LiteralValue, TableRow},
    table::Table,
};

use super::addr::StreamAddr;

pub type StreamSessionTable<ST> = Table<Session<ST>>;

#[derive(Debug, Clone)]
pub struct Session<ST> {
    pub start: SystemTime,
    pub end: Option<SystemTime>,
    pub destination: Option<StreamAddr<ST>>,
    pub upstream_local: Option<SocketAddr>,
    pub upstream_remote: StreamAddr<ST>,
    pub downstream_remote: Option<SocketAddr>,
}

impl<ST: fmt::Display> TableRow for Session<ST> {
    fn schema() -> Vec<(String, LiteralType)> {
        vec![
            (String::from("destination"), LiteralType::String),
            (String::from("duration"), LiteralType::Int),
            (String::from("start_ms"), LiteralType::Int),
            (String::from("end_ms"), LiteralType::Int),
            (String::from("upstream_local"), LiteralType::String),
            (String::from("upstream_remote"), LiteralType::String),
            (String::from("downstream_remote"), LiteralType::String),
        ]
    }

    fn fields(&self) -> Vec<Option<LiteralValue>> {
        let start_unix = self.start.duration_since(UNIX_EPOCH).unwrap();
        let now_unix = SystemTime::now().duration_since(UNIX_EPOCH).unwrap();

        let duration = match self.end {
            Some(end) => end
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .saturating_sub(start_unix),
            None => now_unix.saturating_sub(start_unix),
        };

        let destination = self.destination.as_ref().map(|d| d.to_string().into());
        let duration = Some((duration.as_millis() as i64).into());
        let start = Some((start_unix.as_millis() as i64).into());
        let end = self
            .end
            .map(|e| (e.duration_since(UNIX_EPOCH).unwrap().as_millis() as i64).into());
        let upstream_local = self.upstream_local.map(|x| x.to_string().into());
        let upstream_remote = Some(self.upstream_remote.to_string().into());
        let downstream_remote = self.downstream_remote.map(|x| x.to_string().into());

        vec![
            destination,
            duration,
            start,
            end,
            upstream_local,
            upstream_remote,
            downstream_remote,
        ]
    }

    fn display_value(header: &str, value: Option<LiteralValue>) -> String {
        let Some(v) = value else {
            return String::new();
        };
        match header {
            "duration" => {
                let duration: i64 = v.try_into().unwrap();
                let duration = Duration::from_millis(duration as _);
                if duration.as_secs() == 0 {
                    format!("{} ms", duration.as_millis())
                } else if duration.as_secs() / 60 == 0 {
                    format!("{} s", duration.as_secs())
                } else if duration.as_secs() / 60 / 60 == 0 {
                    format!("{} min", duration.as_secs() / 60)
                } else {
                    format!("{} h", duration.as_secs() / 60 / 60)
                }
            }
            _ => v.to_string(),
        }
    }
}
