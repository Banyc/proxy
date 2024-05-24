use std::{
    net::SocketAddr,
    sync::Mutex,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use bytesize::ByteSize;
use hdv_derive::HdvSerde;
use monitor_table::{
    row::{LiteralType, LiteralValue, TableRow, ValueDisplay},
    table::Table,
};
use tokio_throughput::GaugeHandle;

use crate::addr::{InternetAddr, InternetAddrHdv};

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
        <SessionHdv as TableRow>::schema()
    }

    fn fields(&self) -> Vec<Option<LiteralValue>> {
        let view = SessionHdv::from_session(self);
        TableRow::fields(&view)
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
            "bytes" | "up.bytes" | "dn.bytes" => {
                let bytes = match v {
                    LiteralValue::Int(bytes) => bytes as u64,
                    LiteralValue::UInt(bytes) => bytes,
                    LiteralValue::Float(bytes) => bytes as u64,
                    _ => return v.to_string(),
                };
                ByteSize(bytes).to_string()
            }
            "thruput" | "up.thruput" | "dn.thruput" => {
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

#[derive(Debug, HdvSerde)]
struct SessionHdv {
    pub destination: Option<InternetAddrHdv>,
    pub duration: u64,
    pub start_ms: u64,
    pub end_ms: Option<u64>,
    pub upstream_local: Option<InternetAddrHdv>,
    pub upstream_remote: InternetAddrHdv,
    pub downstream_remote: InternetAddrHdv,
    pub up: GaugeHdv,
    pub dn: GaugeHdv,
}
impl SessionHdv {
    pub fn from_session(s: &Session) -> Self {
        let start_unix = s.start.duration_since(UNIX_EPOCH).unwrap();
        let now_unix = SystemTime::now().duration_since(UNIX_EPOCH).unwrap();

        let duration = match s.end {
            Some(end) => end
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .saturating_sub(start_unix),
            None => now_unix.saturating_sub(start_unix),
        };

        let destination = s.destination.as_ref().map(|d| d.into());
        let duration = duration.as_millis() as u64;
        let start_ms = start_unix.as_millis() as u64;
        let end_ms = s
            .end
            .map(|e| e.duration_since(UNIX_EPOCH).unwrap().as_millis() as u64);
        let upstream_local = s.upstream_local.map(|x| x.into());
        let upstream_remote = (&s.upstream_remote).into();
        let downstream_remote = s.downstream_remote.into();
        let now = Instant::now();
        let up = GaugeHdv::from_gauge_handle(&s.up_gauge, now);
        let dn = GaugeHdv::from_gauge_handle(&s.dn_gauge, now);

        Self {
            destination,
            duration,
            start_ms,
            end_ms,
            upstream_local,
            upstream_remote,
            downstream_remote,
            up,
            dn,
        }
    }
}

#[derive(Debug, HdvSerde)]
struct GaugeHdv {
    pub thruput: f64,
    pub bytes: u64,
}
impl GaugeHdv {
    pub fn from_gauge_handle(g: &Mutex<tokio_throughput::GaugeHandle>, now: Instant) -> Self {
        let mut g = g.lock().unwrap();
        g.update(now);
        Self {
            thruput: g.thruput(),
            bytes: g.total_bytes(),
        }
    }
}
