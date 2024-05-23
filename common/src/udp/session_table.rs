use std::{
    net::SocketAddr,
    sync::Mutex,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use bytesize::ByteSize;
use monitor_table::{
    row::{LiteralType, LiteralValue, TableRow, ValueDisplay},
    table::Table,
};
use tokio_throughput::GaugeHandle;

use crate::addr::InternetAddr;

pub type UdpSessionTable = Table<Session>;

#[derive(Debug)]
pub struct Session {
    pub start: SystemTime,
    pub end: Option<SystemTime>,
    pub destination: Option<InternetAddr>,
    pub upstream_local: Option<SocketAddr>,
    pub upstream_remote: InternetAddr,
    pub downstream_remote: SocketAddr,
    pub up_gauge: Mutex<GaugeHandle>,
    pub dn_gauge: Mutex<GaugeHandle>,
}
impl TableRow for Session {
    fn schema() -> Vec<(String, LiteralType)> {
        vec![
            (String::from("destination"), LiteralType::String),
            (String::from("duration"), LiteralType::Int),
            (String::from("start_ms"), LiteralType::Int),
            (String::from("end_ms"), LiteralType::Int),
            (String::from("upstream_local"), LiteralType::String),
            (String::from("upstream_remote"), LiteralType::String),
            (String::from("downstream_remote"), LiteralType::String),
            (String::from("up_thruput"), LiteralType::Float),
            (String::from("dn_thruput"), LiteralType::Float),
            (String::from("up_bytes"), LiteralType::Int),
            (String::from("dn_bytes"), LiteralType::Int),
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
        let downstream_remote = Some(self.downstream_remote.to_string().into());
        let read_gauge = |g: &Mutex<tokio_throughput::GaugeHandle>| {
            let mut g = g.lock().unwrap();
            g.update(Instant::now());
            (
                Some(g.thruput().into()),
                Some((g.total_bytes() as i64).into()),
            )
        };
        let (up_thruput, up_total_bytes) = read_gauge(&self.up_gauge);
        let (dn_thruput, dn_total_bytes) = read_gauge(&self.dn_gauge);

        vec![
            destination,
            duration,
            start,
            end,
            upstream_local,
            upstream_remote,
            downstream_remote,
            up_thruput,
            dn_thruput,
            up_total_bytes,
            dn_total_bytes,
        ]
    }
}
impl ValueDisplay for Session {
    fn display_value(header: &str, value: Option<LiteralValue>) -> String {
        let Some(v) = value else {
            return String::new();
        };
        match header {
            "dur" | "duration" => {
                let duration = match v {
                    LiteralValue::Int(duration) => duration as u64,
                    LiteralValue::UInt(duration) => duration,
                    LiteralValue::Float(duration) => duration as u64,
                    _ => return v.to_string(),
                };
                let duration = Duration::from_millis(duration);
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
            "bytes" | "up_bytes" | "dn_bytes" => {
                let bytes = match v {
                    LiteralValue::Int(bytes) => bytes as u64,
                    LiteralValue::UInt(bytes) => bytes,
                    LiteralValue::Float(bytes) => bytes as u64,
                    _ => return v.to_string(),
                };
                ByteSize(bytes).to_string()
            }
            "thruput" | "up_thruput" | "dn_thruput" => {
                let thruput = match v {
                    LiteralValue::Int(thruput) => thruput as f64,
                    LiteralValue::UInt(thruput) => thruput as f64,
                    LiteralValue::Float(thruput) => thruput,
                    _ => return v.to_string(),
                };
                if thruput / 1024.0 < 1.0 {
                    format!("{:.1} B/s", thruput)
                } else if thruput / 1024.0 / 1024.0 < 1.0 {
                    format!("{:.1} KB/s", thruput / 1024.0)
                } else if thruput / 1024.0 / 1024.0 / 1024.0 < 1.0 {
                    format!("{:.1} MB/s", thruput / 1024.0 / 1024.0)
                } else {
                    format!("{:.1} GB/s", thruput / 1024.0 / 1024.0 / 1024.0)
                }
            }
            _ => v.to_string(),
        }
    }
}
