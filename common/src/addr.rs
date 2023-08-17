use std::{
    fmt::Display,
    io,
    net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, SocketAddrV4},
    num::NonZeroUsize,
    sync::{Arc, Mutex},
};

use lazy_static::lazy_static;
use lru::LruCache;
use serde::{Deserialize, Serialize};
use tokio::net::lookup_host;

lazy_static! {
    static ref RESOLVED_SOCKET_ADDR: Arc<Mutex<LruCache<Arc<str>, SocketAddr>>> =
        Arc::new(Mutex::new(LruCache::new(NonZeroUsize::new(128).unwrap())));
}

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
            Self::String(host) => {
                let res = lookup_host(host.as_ref())
                    .await?
                    .next()
                    .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "No address"));

                match &res {
                    Ok(addr) => {
                        if let Ok(mut store) = RESOLVED_SOCKET_ADDR.try_lock() {
                            store.put(host.clone(), *addr);
                        }
                    }
                    Err(_) => {
                        let mut store = RESOLVED_SOCKET_ADDR.lock().unwrap();
                        if let Some(addr) = store.get(host) {
                            return Ok(*addr);
                        }
                    }
                }
                res
            }
        }
    }
}
