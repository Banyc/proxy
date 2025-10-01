use std::{
    net::SocketAddr,
    sync::Mutex,
    time::{Instant, SystemTime, UNIX_EPOCH},
};

use hdv_derive::HdvSerde;
use monitor_table::{
    row::{LiteralType, LiteralValue, TableRow, ValueDisplay},
    table::Table,
};
use tokio_throughput::GaugeHandle;

use crate::{
    addr::{InternetAddr, InternetAddrHdv},
    metrics::{GaugeView, display_value},
};

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
        display_value(header, value)
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
    pub up: GaugeView,
    pub dn: GaugeView,
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
        let up = GaugeView::from_gauge_handle(&s.up_gauge, now);
        let dn = GaugeView::from_gauge_handle(&s.dn_gauge, now);

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
