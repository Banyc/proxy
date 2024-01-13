use std::{
    fmt::{self, Display},
    net::SocketAddr,
    ops::Deref,
    time::{Instant, SystemTime, UNIX_EPOCH},
};

use bytesize::ByteSize;

use crate::addr::{InternetAddr, InternetAddrKind};

use super::addr::StreamAddr;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct StreamMetrics<ST> {
    pub start: (Instant, SystemTime),
    pub end: Instant,
    pub bytes_uplink: u64,
    pub bytes_downlink: u64,
    pub upstream_addr: StreamAddr<ST>,
    pub upstream_sock_addr: SocketAddr,
    pub downstream_addr: Option<SocketAddr>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct SimplifiedStreamMetrics<ST> {
    pub start: (Instant, SystemTime),
    pub end: Instant,
    pub upstream_addr: StreamAddr<ST>,
    pub upstream_sock_addr: SocketAddr,
    pub downstream_addr: Option<SocketAddr>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct StreamProxyMetrics<ST> {
    pub stream: StreamMetrics<ST>,
    pub destination: InternetAddr,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct SimplifiedStreamProxyMetrics<ST> {
    pub stream: SimplifiedStreamMetrics<ST>,
    pub destination: InternetAddr,
}

pub enum StreamRecord<'caller, ST> {
    Metrics(&'caller StreamMetrics<ST>),
    ProxyMetrics(&'caller StreamProxyMetrics<ST>),
    SimplifiedMetrics(&'caller SimplifiedStreamMetrics<ST>),
    SimplifiedProxyMetrics(&'caller SimplifiedStreamProxyMetrics<ST>),
}
impl<ST: fmt::Display> serde::Serialize for StreamRecord<'_, ST> {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        #[derive(serde::Serialize)]
        struct Record {
            pub start_ms: u128,
            pub duration_ms: u128,
            pub bytes_uplink: Option<u64>,
            pub bytes_downlink: Option<u64>,
            pub upstream_addr_type: String,
            pub upstream_addr: String,
            pub upstream_sock_addr: SocketAddr,
            pub downstream_addr: Option<SocketAddr>,
            pub destination: Option<String>,
        }
        fn from_metrics<ST: fmt::Display>(s: &StreamMetrics<ST>) -> Record {
            let duration = s.end - s.start.0;
            Record {
                start_ms: s.start.1.duration_since(UNIX_EPOCH).unwrap().as_millis(),
                duration_ms: duration.as_millis(),
                bytes_uplink: Some(s.bytes_uplink),
                bytes_downlink: Some(s.bytes_downlink),
                upstream_addr_type: s.upstream_addr.stream_type.to_string(),
                upstream_addr: s.upstream_addr.address.to_string(),
                upstream_sock_addr: s.upstream_sock_addr,
                downstream_addr: s.downstream_addr,
                destination: None,
            }
        }
        fn from_simplified_metrics<ST: fmt::Display>(s: &SimplifiedStreamMetrics<ST>) -> Record {
            let duration = s.end - s.start.0;
            Record {
                start_ms: s.start.1.duration_since(UNIX_EPOCH).unwrap().as_millis(),
                duration_ms: duration.as_millis(),
                bytes_uplink: None,
                bytes_downlink: None,
                upstream_addr_type: s.upstream_addr.stream_type.to_string(),
                upstream_addr: s.upstream_addr.address.to_string(),
                upstream_sock_addr: s.upstream_sock_addr,
                downstream_addr: s.downstream_addr,
                destination: None,
            }
        }
        let record = match &self {
            Self::Metrics(s) => from_metrics(s),
            Self::ProxyMetrics(s) => {
                let mut r = from_metrics(&s.stream);
                r.destination = Some(s.destination.to_string());
                r
            }
            Self::SimplifiedMetrics(s) => from_simplified_metrics(s),
            Self::SimplifiedProxyMetrics(s) => {
                let mut r = from_simplified_metrics(&s.stream);
                r.destination = Some(s.destination.to_string());
                r
            }
        };
        record.serialize(serializer)
    }
}
impl<'erased, ST: fmt::Display + 'erased> table_log::LogRecord<'erased>
    for StreamRecord<'erased, ST>
{
    fn table_name(&self) -> &'static str {
        "stream_record"
    }
}

impl<ST: Display> Display for StreamMetrics<ST> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let duration = self.end - self.start.0;
        let duration = duration.as_secs_f64();
        let uplink_speed = self.bytes_uplink as f64 / duration;
        let downlink_speed = self.bytes_downlink as f64 / duration;
        let upstream_addrs = match self.upstream_addr.address.deref() {
            InternetAddrKind::SocketAddr(_) => self.upstream_addr.to_string(),
            InternetAddrKind::DomainName { .. } => {
                format!("{},{}", self.upstream_addr, self.upstream_sock_addr.ip())
            }
        };
        write!(
            f,
            "{:.1}s,up{{{},{}/s}},dn{{{},{}/s}},up{{{}}}",
            duration,
            ByteSize::b(self.bytes_uplink),
            ByteSize::b(uplink_speed as u64),
            ByteSize::b(self.bytes_downlink),
            ByteSize::b(downlink_speed as u64),
            upstream_addrs,
        )?;
        if let Some(downstream_addr) = self.downstream_addr {
            write!(f, ",dn:{}", downstream_addr)?;
        }
        Ok(())
    }
}

impl<ST: Display> Display for SimplifiedStreamMetrics<ST> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let duration = self.end - self.start.0;
        let duration = duration.as_secs_f64();
        let upstream_addrs = match self.upstream_addr.address.deref() {
            InternetAddrKind::SocketAddr(_) => self.upstream_addr.to_string(),
            InternetAddrKind::DomainName { .. } => {
                format!("{},{}", self.upstream_addr, self.upstream_sock_addr.ip())
            }
        };
        write!(f, "{:.1}s,up{{{}}}", duration, upstream_addrs)?;
        if let Some(downstream_addr) = self.downstream_addr {
            write!(f, ",dn:{}", downstream_addr)?;
        }
        Ok(())
    }
}

impl<ST: Display> Display for StreamProxyMetrics<ST> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.stream.to_string())?;
        write!(f, ",dt:{}", self.destination)?;
        Ok(())
    }
}

impl<ST: Display> Display for SimplifiedStreamProxyMetrics<ST> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.stream.to_string())?;
        write!(f, ",dt:{}", self.destination)?;
        Ok(())
    }
}
