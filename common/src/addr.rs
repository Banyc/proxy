use std::{
    fmt::Display,
    io,
    net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, SocketAddrV4},
    sync::Arc,
};

use serde::{Deserialize, Serialize};
use tokio::net::lookup_host;

pub fn any_addr(ip_version: &IpAddr) -> SocketAddr {
    let any_ip = match ip_version {
        IpAddr::V4(_) => Ipv4Addr::UNSPECIFIED.into(),
        IpAddr::V6(_) => Ipv6Addr::UNSPECIFIED.into(),
    };
    SocketAddr::new(any_ip, 0)
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Deserialize, Serialize)]
pub enum InternetAddr {
    SocketAddr(SocketAddr),
    String(Arc<str>),
}

impl Display for InternetAddr {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::SocketAddr(addr) => write!(f, "{}", addr),
            Self::String(string) => write!(f, "{}", string),
        }
    }
}

impl From<SocketAddr> for InternetAddr {
    fn from(addr: SocketAddr) -> Self {
        Self::SocketAddr(addr)
    }
}

impl From<Arc<str>> for InternetAddr {
    fn from(string: Arc<str>) -> Self {
        match string.parse::<SocketAddr>() {
            Ok(addr) => Self::SocketAddr(addr),
            Err(_) => Self::String(string),
        }
    }
}

impl InternetAddr {
    pub fn zero_ipv4_addr() -> Self {
        Self::SocketAddr(SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, 0).into())
    }

    pub async fn to_socket_addr(&self) -> io::Result<SocketAddr> {
        match self {
            Self::SocketAddr(addr) => Ok(*addr),
            Self::String(host) => lookup_host(host.as_ref())
                .await?
                .next()
                .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "No address")),
        }
    }
}
