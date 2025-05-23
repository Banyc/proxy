use std::{
    fmt::Display,
    io,
    net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, SocketAddrV4},
    ops::Deref,
    str::FromStr,
    sync::{Arc, LazyLock, Mutex},
};

use hdv_derive::HdvSerde;
use primitive::map::{MapInsert, hash_map::HashGetMut, weak_lru::WeakLru};
use serde::{Deserialize, Serialize, de::Visitor};
use thiserror::Error;
use tokio::net::lookup_host;

use crate::route::IntoAddr;

const RESOLVED_SOCKET_ADDR_SIZE: usize = 128;
static RESOLVED_SOCKET_ADDR: LazyLock<Mutex<WeakLru<Arc<str>, IpAddr, RESOLVED_SOCKET_ADDR_SIZE>>> =
    LazyLock::new(|| Mutex::new(WeakLru::new()));

#[derive(Debug, Clone, Copy, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct BothVerIp {
    pub v4: Option<Ipv4Addr>,
    pub v6: Option<Ipv6Addr>,
}
impl BothVerIp {
    pub fn get_matched(&self, ip_version: &IpAddr) -> Option<IpAddr> {
        Some(match ip_version {
            IpAddr::V4(_) => self.v4?.into(),
            IpAddr::V6(_) => self.v6?.into(),
        })
    }
}

pub fn any_addr(ip_version: &IpAddr) -> SocketAddr {
    let any_ip = match ip_version {
        IpAddr::V4(_) => Ipv4Addr::UNSPECIFIED.into(),
        IpAddr::V6(_) => Ipv6Addr::UNSPECIFIED.into(),
    };
    SocketAddr::new(any_ip, 0)
}

#[derive(
    Debug, Clone, PartialEq, Eq, Hash, Deserialize, Serialize, bincode::Encode, bincode::Decode,
)]
pub struct InternetAddr(InternetAddrKind);
impl Deref for InternetAddr {
    type Target = InternetAddrKind;

    fn deref(&self) -> &Self::Target {
        let Self(kind) = self;
        kind
    }
}
impl Display for InternetAddr {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self(InternetAddrKind::SocketAddr(addr)) => write!(f, "{addr}"),
            Self(InternetAddrKind::DomainName { addr, port }) => write!(f, "{addr}:{port}",),
        }
    }
}
impl From<SocketAddr> for InternetAddr {
    fn from(addr: SocketAddr) -> Self {
        Self(InternetAddrKind::SocketAddr(addr))
    }
}
impl FromStr for InternetAddr {
    type Err = ParseInternetAddrError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        if let Ok(addr) = s.parse::<SocketAddr>() {
            return Ok(Self(InternetAddrKind::SocketAddr(addr)));
        }

        let mut parts = s.split(':');
        let addr = parts.next().ok_or(ParseInternetAddrError)?.into();
        let port = parts.next().ok_or(ParseInternetAddrError)?;
        let port = port.parse().map_err(|_| ParseInternetAddrError)?;
        if parts.next().is_some() {
            return Err(ParseInternetAddrError);
        }
        Ok(Self(InternetAddrKind::DomainName { addr, port }))
    }
}

#[derive(
    Debug, Clone, PartialEq, Eq, Hash, Deserialize, Serialize, bincode::Encode, bincode::Decode,
)]
#[serde(deny_unknown_fields)]
pub enum InternetAddrKind {
    SocketAddr(SocketAddr),
    DomainName { addr: Arc<str>, port: u16 },
}

#[derive(Debug, Error, Clone, Copy)]
#[error("Failed to parse Internet address")]
pub struct ParseInternetAddrError;

impl InternetAddr {
    pub fn zero_ipv4_addr() -> Self {
        Self(InternetAddrKind::SocketAddr(
            SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, 0).into(),
        ))
    }

    pub fn from_host_and_port<H>(host: H, port: u16) -> Result<Self, ParseInternetAddrError>
    where
        H: Into<Arc<str>> + AsRef<str>,
    {
        if let Ok(ip) = host.as_ref().parse::<IpAddr>() {
            return Ok(Self(InternetAddrKind::SocketAddr(SocketAddr::new(
                ip, port,
            ))));
        }

        if host.as_ref().contains(':') {
            return Err(ParseInternetAddrError);
        }
        Ok(Self(InternetAddrKind::DomainName {
            addr: host.into(),
            port,
        }))
    }

    pub fn port(&self) -> u16 {
        match self.deref() {
            InternetAddrKind::SocketAddr(s) => s.port(),
            InternetAddrKind::DomainName { port, .. } => *port,
        }
    }

    pub async fn to_socket_addr(&self) -> io::Result<SocketAddr> {
        match self {
            Self(InternetAddrKind::SocketAddr(addr)) => Ok(*addr),
            Self(InternetAddrKind::DomainName { addr, port }) => {
                let res = lookup_host((addr.as_ref(), *port))
                    .await
                    .and_then(|mut res| {
                        res.next()
                            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "No address"))
                    });

                match &res {
                    Ok(resolved_addr) => {
                        if let Ok(mut store) = RESOLVED_SOCKET_ADDR.try_lock() {
                            store.insert(addr.clone(), resolved_addr.ip());
                        }
                    }
                    Err(_) => {
                        let mut store = RESOLVED_SOCKET_ADDR.lock().unwrap();
                        if let Some(ip) = store.get_mut(addr.as_ref()) {
                            return Ok(SocketAddr::new(*ip, *port));
                        }
                    }
                }
                res
            }
        }
    }
}

#[derive(Debug, Clone, HdvSerde)]
pub struct InternetAddrHdv {
    pub host: Arc<str>,
    pub port: u16,
}
impl From<&InternetAddr> for InternetAddrHdv {
    fn from(value: &InternetAddr) -> Self {
        let (host, port) = match &value.0 {
            InternetAddrKind::SocketAddr(x) => return (*x).into(),
            InternetAddrKind::DomainName { addr, port } => (addr.clone(), *port),
        };
        Self { host, port }
    }
}
impl From<SocketAddr> for InternetAddrHdv {
    fn from(value: SocketAddr) -> Self {
        let (host, port) = (value.ip().to_string().into(), value.port());
        Self { host, port }
    }
}

#[derive(Debug, Clone)]
pub struct InternetAddrStr(pub InternetAddr);
impl IntoAddr for InternetAddrStr {
    type Addr = InternetAddr;
    fn into_address(self) -> Self::Addr {
        self.0
    }
}
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
impl Visitor<'_> for InternetAddrStrVisitor {
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
            v.0.deref(),
            &InternetAddrKind::SocketAddr("127.0.0.1:1".parse().unwrap())
        );
        let new_s = serde_json::to_string(&v).unwrap();
        assert_eq!(s, new_s);
    }

    #[test]
    fn serde_domain_name() {
        let s = "\"example.website:1\"";
        let v: InternetAddrStr = serde_json::from_str(s).unwrap();
        assert_eq!(
            v.0.deref(),
            &InternetAddrKind::DomainName {
                addr: "example.website".into(),
                port: 1
            }
        );
        let new_s = serde_json::to_string(&v).unwrap();
        assert_eq!(s, new_s);
    }
}
