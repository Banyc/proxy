use std::{
    fmt::Display,
    io,
    net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, SocketAddrV4},
    ops::Deref,
    str::FromStr,
    sync::{Arc, LazyLock, Mutex},
};

use hdv_derive::HdvSerde;
use mitsein::prelude::*;
use primitive::map::{MapInsert, hash_map::HashGetMut, weak_lru::WeakLru};
use serde::{Deserialize, Serialize, de::Visitor};
use thiserror::Error;
use tokio::net::lookup_host;

const RESOLVED_SOCKET_ADDR_SIZE: usize = 128;
static RESOLVED_SOCKET_ADDR: LazyLock<
    Mutex<WeakLru<Arc<str>, Vec<IpAddr>, RESOLVED_SOCKET_ADDR_SIZE>>,
> = LazyLock::new(|| Mutex::new(WeakLru::new()));

#[derive(Debug, Clone, Copy, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct DualStackBind {
    pub v4: Option<Ipv4Addr>,
    pub v6: Option<Ipv6Addr>,
}
impl DualStackBind {
    pub fn get_matched(&self, ip_version: &IpAddr) -> Option<IpAddr> {
        Some(match ip_version {
            IpAddr::V4(_) => self.v4?.into(),
            IpAddr::V6(_) => self.v6?.into(),
        })
    }
}
pub fn reaches_loopback(ip: &IpAddr) -> bool {
    if ip.is_loopback() || ip.is_unspecified() {
        return true;
    }
    match ip {
        IpAddr::V4(_) => false,
        IpAddr::V6(ip) => ip
            .to_ipv4()
            .is_some_and(|ip| ip.is_loopback() || ip.is_unspecified()),
    }
}

pub fn any_addr(ip_version: &IpAddr) -> SocketAddr {
    let any_ip = match ip_version {
        IpAddr::V4(_) => Ipv4Addr::UNSPECIFIED.into(),
        IpAddr::V6(_) => Ipv6Addr::UNSPECIFIED.into(),
    };
    SocketAddr::new(any_ip, 0)
}

/// The address to actually dial for `addr`. An unspecified address
/// (`0.0.0.0` / `::`) names no host but is routinely used as a listener
/// bind address returned by `local_addr()`; dialing it verbatim fails
/// (`EHOSTUNREACH` for UDP on macOS) instead of reaching the local host.
/// Canonicalize it to the matching loopback address so a client may dial
/// a bound listener's reported address, mirroring the RTP transport's
/// dial normalization.
pub fn dialable_addr(addr: SocketAddr) -> SocketAddr {
    if !addr.ip().is_unspecified() {
        return addr;
    }
    let loopback = match addr.ip() {
        IpAddr::V4(_) => IpAddr::V4(Ipv4Addr::LOCALHOST),
        IpAddr::V6(_) => IpAddr::V6(Ipv6Addr::LOCALHOST),
    };
    SocketAddr::new(loopback, addr.port())
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Deserialize, Serialize)]
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

#[derive(Debug, Clone, PartialEq, Eq, Hash, Deserialize, Serialize)]
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

    pub async fn to_socket_addrs(&self) -> io::Result<Vec1<SocketAddr>> {
        match self {
            Self(InternetAddrKind::SocketAddr(addr)) => Ok(vec1![*addr]),
            Self(InternetAddrKind::DomainName { addr, port }) => {
                let no_addr_err = || io::Error::new(io::ErrorKind::InvalidData, "No address");
                let res = lookup_host((addr.as_ref(), *port))
                    .await
                    .map(|res| res.collect::<Vec<SocketAddr>>())
                    .and_then(|addrs| Vec1::try_from(addrs).map_err(|_| no_addr_err()));
                match &res {
                    Ok(resolved_addrs) => {
                        if let Ok(mut store) = RESOLVED_SOCKET_ADDR.try_lock() {
                            let ips = resolved_addrs.iter().map(SocketAddr::ip).collect();
                            store.insert(addr.clone(), ips);
                        }
                    }
                    Err(_) => {
                        let mut store = RESOLVED_SOCKET_ADDR.lock().unwrap();
                        if let Some(ips) = store.get_mut(addr.as_ref()) {
                            let addrs = ips
                                .iter()
                                .copied()
                                .map(|ip| SocketAddr::new(ip, *port))
                                .collect::<Vec<_>>();
                            return Vec1::try_from(addrs).map_err(|_| no_addr_err());
                        }
                    }
                }
                res
            }
        }
    }
}

#[derive(Debug, Clone, HdvSerde)]
pub struct InternetAddrHostPort {
    pub host: Arc<str>,
    pub port: u16,
}
impl From<&InternetAddr> for InternetAddrHostPort {
    fn from(value: &InternetAddr) -> Self {
        let (host, port) = match &value.0 {
            InternetAddrKind::SocketAddr(x) => return (*x).into(),
            InternetAddrKind::DomainName { addr, port } => (addr.clone(), *port),
        };
        Self { host, port }
    }
}
impl From<SocketAddr> for InternetAddrHostPort {
    fn from(value: SocketAddr) -> Self {
        let (host, port) = (value.ip().to_string().into(), value.port());
        Self { host, port }
    }
}

#[derive(Debug, Clone)]
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
    fn a_loopback_address_spelled_as_ipv6_still_reaches_loopback() {
        for ip in [
            "127.0.0.1",
            "127.1.2.3",
            "::1",
            "::ffff:127.0.0.1",
            "::127.0.0.1",
            "0.0.0.0",
            "::",
            "::ffff:0.0.0.0",
        ] {
            let ip: IpAddr = ip.parse().unwrap();
            assert!(reaches_loopback(&ip), "{ip}");
        }
        for ip in ["1.1.1.1", "::ffff:1.1.1.1", "2606:4700:4700::1111"] {
            let ip: IpAddr = ip.parse().unwrap();
            assert!(!reaches_loopback(&ip), "{ip}");
        }
    }

    #[test]
    fn an_unspecified_dial_address_is_canonicalized_to_loopback() {
        for (input, expected) in [
            ("0.0.0.0:80", "127.0.0.1:80"),
            ("[::]:80", "[::1]:80"),
            ("127.0.0.1:80", "127.0.0.1:80"),
            ("[::1]:80", "[::1]:80"),
            ("1.1.1.1:80", "1.1.1.1:80"),
        ] {
            let input: SocketAddr = input.parse().unwrap();
            let expected: SocketAddr = expected.parse().unwrap();
            assert_eq!(dialable_addr(input), expected, "{input}");
        }
    }

    #[test]
    fn a_dual_stack_bind_selects_the_family_matching_the_dial_target() {
        // The connector picks the bind address for the family it is about to
        // dial; returning the other family's address silently binds the
        // wrong socket (or none) for every configured dual-stack bind.
        let both = DualStackBind {
            v4: Some("192.0.2.1".parse().unwrap()),
            v6: Some("2001:db8::1".parse().unwrap()),
        };
        assert_eq!(
            both.get_matched(&"203.0.113.9".parse().unwrap()),
            Some("192.0.2.1".parse::<IpAddr>().unwrap()),
            "an IPv4 target must select the IPv4 bind"
        );
        assert_eq!(
            both.get_matched(&"2001:db8::2".parse().unwrap()),
            Some("2001:db8::1".parse::<IpAddr>().unwrap()),
            "an IPv6 target must select the IPv6 bind"
        );
        // A family with no configured bind yields no bind for that family,
        // never the other family's address.
        let v4_only = DualStackBind {
            v4: Some("192.0.2.1".parse().unwrap()),
            v6: None,
        };
        assert_eq!(v4_only.get_matched(&"2001:db8::2".parse().unwrap()), None);
        let v6_only = DualStackBind {
            v4: None,
            v6: Some("2001:db8::1".parse().unwrap()),
        };
        assert_eq!(v6_only.get_matched(&"203.0.113.9".parse().unwrap()), None);
    }

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
    fn a_host_containing_a_colon_is_rejected_instead_of_becoming_a_domain_name() {
        // A domain name cannot contain ':', and letting one through would
        // smuggle host/port structure into the domain string (e.g. from a
        // SOCKS5 domain-name field). Such a host must be rejected, not
        // silently accepted as a domain.
        assert!(InternetAddr::from_host_and_port("example.website", 80).is_ok());
        assert!(InternetAddr::from_host_and_port("evil:80", 80).is_err());
        assert!(InternetAddr::from_host_and_port("http://evil", 80).is_err());
    }

    /// `from_str` accepts exactly two shapes: a `SocketAddr`, or a
    /// `host:port` whose host is a domain. A string with a third colon is
    /// neither — the `SocketAddr` parse has already failed, and accepting it
    /// as a domain would silently discard everything after the port and turn
    /// a malformed address into a plausible-looking one. This is the same
    /// rejection `from_host_and_port` pins for a colon-bearing host, on the
    /// `from_str` path that config-file addresses use.
    #[test]
    fn a_string_with_a_third_colon_that_is_not_a_socket_addr_is_rejected() {
        for s in [
            "1.2.3.4:80:90",
            "example.website:80:443",
            "example.website:80:x",
        ] {
            assert!(
                s.parse::<InternetAddr>().is_err(),
                "`{s}` is neither a socket address nor a `host:port` pair and must be rejected"
            );
        }
        // The two accepted shapes stay accepted (and keep their port).
        assert_eq!("1.2.3.4:80".parse::<InternetAddr>().unwrap().port(), 80);
        assert_eq!(
            "example.website:80".parse::<InternetAddr>().unwrap().port(),
            80
        );
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
