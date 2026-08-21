use std::{
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

use crate::{
    addr::InternetAddrHostPort,
    metrics::{GaugeView, display_value},
    proxy_runtime::addr::{RouteAddr, RouteAddrHdv},
};

pub type StreamSessionTable = Table<StreamSession>;

#[derive(Debug)]
pub struct StreamSession {
    pub start: SystemTime,
    pub end: Option<SystemTime>,
    pub destination: Option<RouteAddr>,
    pub upstream_local: Option<SocketAddr>,
    pub upstream_remote: RouteAddr,
    pub downstream_local: Arc<str>,
    pub downstream_remote: Option<SocketAddr>,
    pub up_gauge: Option<Mutex<GaugeHandle>>,
    pub dn_gauge: Option<Mutex<GaugeHandle>>,
}
impl TableRow for StreamSession {
    fn schema() -> Vec<(String, LiteralType)> {
        <StreamSessionView as TableRow>::schema()
    }

    fn fields(&self) -> Vec<Option<LiteralValue>> {
        let view = StreamSessionView::from_stream_session(self);
        TableRow::fields(&view)
    }
}
impl ValueDisplay for StreamSession {
    fn display_value(header: &str, value: Option<LiteralValue>) -> String {
        display_value(header, value)
    }
}

#[derive(Debug, HdvSerde)]
struct StreamSessionView {
    pub destination: Option<RouteAddrHdv>,
    pub duration: u64,
    pub start_ms: u64,
    pub end_ms: Option<u64>,
    pub upstream_local: Option<InternetAddrHostPort>,
    pub upstream_remote: RouteAddrHdv,
    pub downstream_local: Arc<str>,
    pub downstream_remote: Option<InternetAddrHostPort>,
    pub up: Option<GaugeView>,
    pub dn: Option<GaugeView>,
}
impl StreamSessionView {
    pub fn from_stream_session(s: &StreamSession) -> Self {
        let start_unix = s.start.duration_since(UNIX_EPOCH).unwrap_or_default();
        let now_unix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default();

        let duration = match s.end {
            Some(end) => end
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .saturating_sub(start_unix),
            None => now_unix.saturating_sub(start_unix),
        };

        let destination = s.destination.as_ref().map(|d| d.into());
        let duration = duration.as_millis() as u64;
        let start_ms = start_unix.as_millis() as u64;
        let end_ms = s
            .end
            .map(|e| e.duration_since(UNIX_EPOCH).unwrap_or_default().as_millis() as u64);
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
