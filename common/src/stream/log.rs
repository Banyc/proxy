use std::{
    fmt::{self, Display},
    net::SocketAddr,
    ops::Deref,
    path::PathBuf,
    sync::{Arc, LazyLock, Mutex},
};

use hdv_derive::HdvSerde;
use primitive::ops::unit::HumanBytes;

use crate::{
    addr::{InternetAddr, InternetAddrHdv, InternetAddrKind},
    log::{HdvLogger, Timing, TimingHdv},
};

use super::addr::{StreamAddr, StreamAddrHdv};

pub static LOGGER: LazyLock<Arc<Mutex<Option<HdvLogger<StreamLogHdv>>>>> =
    LazyLock::new(|| Arc::new(Mutex::new(None)));
pub fn init_logger(output_dir: PathBuf) {
    let output_dir = output_dir.join("stream_record");
    let logger = HdvLogger::new(output_dir);
    *LOGGER.lock().unwrap() = Some(logger);
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct StreamLog<ST> {
    pub timing: Timing,
    pub bytes_uplink: u64,
    pub bytes_downlink: u64,
    pub upstream_addr: StreamAddr<ST>,
    pub upstream_sock_addr: SocketAddr,
    pub downstream_addr: Option<SocketAddr>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct SimplifiedStreamLog<ST> {
    pub timing: Timing,
    pub upstream_addr: StreamAddr<ST>,
    pub upstream_sock_addr: SocketAddr,
    pub downstream_addr: Option<SocketAddr>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct StreamProxyLog<ST> {
    pub stream: StreamLog<ST>,
    pub destination: InternetAddr,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct SimplifiedStreamProxyLog<ST> {
    pub stream: SimplifiedStreamLog<ST>,
    pub destination: InternetAddr,
}

#[derive(Debug, Clone, HdvSerde)]
pub struct StreamLogHdv {
    pub timing: TimingHdv,
    pub up_bytes: Option<u64>,
    pub dn_bytes: Option<u64>,
    pub upstream_addr: StreamAddrHdv,
    pub upstream_sock_addr: InternetAddrHdv,
    pub downstream_addr: Option<InternetAddrHdv>,
    pub destination: Option<InternetAddrHdv>,
}
impl<ST: fmt::Display> From<&StreamLog<ST>> for StreamLogHdv {
    fn from(value: &StreamLog<ST>) -> Self {
        let timing = (&value.timing).into();
        let up_bytes = Some(value.bytes_uplink);
        let dn_bytes = Some(value.bytes_downlink);
        let upstream_addr = (&value.upstream_addr).into();
        let upstream_sock_addr = value.upstream_sock_addr.into();
        let downstream_addr = value.downstream_addr.map(|x| x.into());
        let destination = None;
        Self {
            timing,
            up_bytes,
            dn_bytes,
            upstream_addr,
            upstream_sock_addr,
            downstream_addr,
            destination,
        }
    }
}
impl<ST: fmt::Display> From<&SimplifiedStreamLog<ST>> for StreamLogHdv {
    fn from(value: &SimplifiedStreamLog<ST>) -> Self {
        let timing = (&value.timing).into();
        let up_bytes = None;
        let dn_bytes = None;
        let upstream_addr = (&value.upstream_addr).into();
        let upstream_sock_addr = value.upstream_sock_addr.into();
        let downstream_addr = value.downstream_addr.map(|x| x.into());
        let destination = None;
        Self {
            timing,
            up_bytes,
            dn_bytes,
            upstream_addr,
            upstream_sock_addr,
            downstream_addr,
            destination,
        }
    }
}
impl<ST: fmt::Display> From<&StreamProxyLog<ST>> for StreamLogHdv {
    fn from(value: &StreamProxyLog<ST>) -> Self {
        let mut this: StreamLogHdv = (&value.stream).into();
        this.destination = Some((&value.destination).into());
        this
    }
}
impl<ST: fmt::Display> From<&SimplifiedStreamProxyLog<ST>> for StreamLogHdv {
    fn from(value: &SimplifiedStreamProxyLog<ST>) -> Self {
        let mut this: StreamLogHdv = (&value.stream).into();
        this.destination = Some((&value.destination).into());
        this
    }
}

impl<ST: Display> Display for StreamLog<ST> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let duration = self.timing.duration().as_secs_f64();
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
            "{:.1}s,up{{{:.1},{:.1}/s}},dn{{{:.1},{:.1}/s}},up{{{}}}",
            duration,
            HumanBytes(self.bytes_uplink),
            HumanBytes(uplink_speed as u64),
            HumanBytes(self.bytes_downlink),
            HumanBytes(downlink_speed as u64),
            upstream_addrs,
        )?;
        if let Some(downstream_addr) = self.downstream_addr {
            write!(f, ",dn:{}", downstream_addr)?;
        }
        Ok(())
    }
}

impl<ST: Display> Display for SimplifiedStreamLog<ST> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let duration = self.timing.duration().as_secs_f64();
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

impl<ST: Display> Display for StreamProxyLog<ST> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.stream.to_string())?;
        write!(f, ",dt:{}", self.destination)?;
        Ok(())
    }
}

impl<ST: Display> Display for SimplifiedStreamProxyLog<ST> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.stream.to_string())?;
        write!(f, ",dt:{}", self.destination)?;
        Ok(())
    }
}
