use std::{
    fmt,
    net::SocketAddr,
    sync::{Arc, Mutex},
    time::{Instant, SystemTime, UNIX_EPOCH},
};

use hdv_derive::HdvSerde;
use monitor_table::{
    row::{LiteralType, LiteralValue, TableRow, ValueDisplay},
    table::Table,
};
use tokio_throughput::GaugeHandle;

use crate::{addr::InternetAddrHdv, metrics::display_value};

use super::addr::{StreamAddr, StreamAddrHdv};

pub type StreamSessionTable<StreamType> = Table<Session<StreamType>>;

#[derive(Debug)]
pub struct Session<StreamType> {
    pub start: SystemTime,
    pub end: Option<SystemTime>,
    pub destination: Option<StreamAddr<StreamType>>,
    pub upstream_local: Option<SocketAddr>,
    pub upstream_remote: StreamAddr<StreamType>,
    pub downstream_local: Arc<str>,
    pub downstream_remote: Option<SocketAddr>,
    pub up_gauge: Option<Mutex<GaugeHandle>>,
    pub dn_gauge: Option<Mutex<GaugeHandle>>,
}
impl<StreamType: fmt::Display> TableRow for Session<StreamType> {
    fn schema() -> Vec<(String, LiteralType)> {
        <SessionView as TableRow>::schema()
    }

    fn fields(&self) -> Vec<Option<LiteralValue>> {
        let view = SessionView::from_session(self);
        TableRow::fields(&view)
    }
}
impl<StreamType> ValueDisplay for Session<StreamType> {
    fn display_value(header: &str, value: Option<LiteralValue>) -> String {
        display_value(header, value)
    }
}

#[derive(Debug, HdvSerde)]
struct SessionView {
    pub destination: Option<StreamAddrHdv>,
    pub duration: u64,
    pub start_ms: u64,
    pub end_ms: Option<u64>,
    pub upstream_local: Option<InternetAddrHdv>,
    pub upstream_remote: StreamAddrHdv,
    pub downstream_local: Arc<str>,
    pub downstream_remote: Option<InternetAddrHdv>,
    pub up: Option<GaugeView>,
    pub dn: Option<GaugeView>,
}
impl SessionView {
    pub fn from_session<StreamType: fmt::Display>(s: &Session<StreamType>) -> Self {
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
        let downstream_local = Arc::clone(&s.downstream_local);
        let downstream_remote = s.downstream_remote.map(|x| x.into());
        let now = Instant::now();
        let up = s
            .up_gauge
            .as_ref()
            .map(|x| GaugeView::from_gauge_handle(x, now));
        let dn = s
            .dn_gauge
            .as_ref()
            .map(|x| GaugeView::from_gauge_handle(x, now));

        Self {
            destination,
            duration,
            start_ms,
            end_ms,
            upstream_local,
            upstream_remote,
            downstream_local,
            downstream_remote,
            up,
            dn,
        }
    }
}

#[derive(Debug, HdvSerde)]
struct GaugeView {
    pub thruput: f64,
    pub bytes: u64,
}
impl GaugeView {
    pub fn from_gauge_handle(g: &Mutex<tokio_throughput::GaugeHandle>, now: Instant) -> Self {
        let mut g = g.lock().unwrap();
        g.update(now);
        Self {
            thruput: g.thruput(),
            bytes: g.total_bytes(),
        }
    }
}
