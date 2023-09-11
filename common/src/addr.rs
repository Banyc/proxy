use std::{
    fmt::Display,
    io,
    net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, SocketAddrV4},
    num::NonZeroUsize,
    str::FromStr,
    sync::{Arc, Mutex},
};

use lazy_static::lazy_static;
use lru::LruCache;
use serde::{de::Visitor, Deserialize, Serialize};
use thiserror::Error;
use tokio::net::lookup_host;

lazy_static! {
    static ref RESOLVED_SOCKET_ADDR: Arc<Mutex<LruCache<Arc<str>, IpAddr>>> =
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
    DomainName { addr: Arc<str>, port: u16 },
}

impl Display for InternetAddr {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::SocketAddr(addr) => write!(f, "{}", addr),
            Self::DomainName { addr, port } => write!(f, "{addr}:{port}",),
        }
    }
}

impl From<SocketAddr> for InternetAddr {
    fn from(addr: SocketAddr) -> Self {
        Self::SocketAddr(addr)
    }
}

impl FromStr for InternetAddr {
    type Err = ParseInternetAddrError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        if let Ok(addr) = s.parse::<SocketAddr>() {
            return Ok(Self::SocketAddr(addr));
        }

        let mut parts = s.split(':');
        let addr = parts.next().ok_or(ParseInternetAddrError)?.into();
        let port = parts.next().ok_or(ParseInternetAddrError)?;
        let port = port.parse().map_err(|_| ParseInternetAddrError)?;
        if parts.next().is_some() {
            return Err(ParseInternetAddrError);
        }
        Ok(Self::DomainName { addr, port })
    }
}

#[derive(Debug, Error, Clone, Copy)]
#[error("Failed to parse Internet address")]
pub struct ParseInternetAddrError;

impl InternetAddr {
    pub fn zero_ipv4_addr() -> Self {
        Self::SocketAddr(SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, 0).into())
    }

    pub fn port(&self) -> u16 {
        match self {
            InternetAddr::SocketAddr(s) => s.port(),
            InternetAddr::DomainName { port, .. } => *port,
        }
    }

    pub async fn to_socket_addr(&self) -> io::Result<SocketAddr> {
        match self {
            Self::SocketAddr(addr) => Ok(*addr),
            Self::DomainName { addr, port } => {
                let res = lookup_host((addr.as_ref(), *port))
                    .await
                    .and_then(|mut res| {
                        res.next()
                            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "No address"))
                    });

                match &res {
                    Ok(resolved_addr) => {
                        if let Ok(mut store) = RESOLVED_SOCKET_ADDR.try_lock() {
                            store.put(addr.clone(), resolved_addr.ip());
                        }
                    }
                    Err(_) => {
                        let mut store = RESOLVED_SOCKET_ADDR.lock().unwrap();
                        if let Some(ip) = store.get(addr.as_ref()) {
                            return Ok(SocketAddr::new(*ip, *port));
                        }
                    }
                }
                res
            }
        }
    }
}

pub struct InternetAddrStr(pub InternetAddr);

impl Serialize for InternetAddrStr {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_str(&self.0.to_string())
    }
}

impl<'de> Deserialize<'de> for InternetAddrStr {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        deserializer.deserialize_str(InternetAddrStrVisitor)
    }
}

struct InternetAddrStrVisitor;

impl<'de> Visitor<'de> for InternetAddrStrVisitor {
    type Value = InternetAddrStr;

    fn expecting(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
        formatter.write_str("Internet address")
    }

    fn visit_str<E>(self, v: &str) -> Result<Self::Value, E>
    where
        E: serde::de::Error,
    {
        let v: InternetAddr = v.parse().map_err(|e| serde::de::Error::custom(e))?;
        Ok(InternetAddrStr(v))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn serde_socket_address() {
        let s = "\"127.0.0.1:1\"";
        let v: InternetAddrStr = serde_json::from_str(s).unwrap();
        assert_eq!(
            v.0,
            InternetAddr::SocketAddr("127.0.0.1:1".parse().unwrap())
        );
        let new_s = serde_json::to_string(&v).unwrap();
        assert_eq!(s, new_s);
    }

    #[test]
    fn serde_domain_name() {
        let s = "\"example.website:1\"";
        let v: InternetAddrStr = serde_json::from_str(s).unwrap();
        assert_eq!(
            v.0,
            InternetAddr::DomainName {
                addr: "example.website".into(),
                port: 1
            }
        );
        let new_s = serde_json::to_string(&v).unwrap();
        assert_eq!(s, new_s);
    }
}
