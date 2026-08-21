use std::{fmt::Display, str::FromStr, sync::Arc};

use hdv_derive::HdvSerde;
use serde::{Deserialize, Serialize};

use crate::addr::{InternetAddr, InternetAddrHostPort, ParseInternetAddrError};

pub const REVERSE_TUNNEL_TCP_PROTOCOL: &str = "revtuntcp";
pub const REVERSE_TUNNEL_RTP_PROTOCOL: &str = "revtunrtp";
pub const MAX_REVERSE_TUNNEL_NAME_LEN: usize = 128;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ReverseTunnelTransport {
    Tcp,
    Rtp,
}
impl ReverseTunnelTransport {
    pub fn from_protocol(protocol: &str) -> Option<Self> {
        match protocol {
            REVERSE_TUNNEL_TCP_PROTOCOL => Some(Self::Tcp),
            REVERSE_TUNNEL_RTP_PROTOCOL => Some(Self::Rtp),
            _ => None,
        }
    }
    pub fn protocol(self) -> &'static str {
        match self {
            Self::Tcp => REVERSE_TUNNEL_TCP_PROTOCOL,
            Self::Rtp => REVERSE_TUNNEL_RTP_PROTOCOL,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Deserialize, Serialize)]
pub struct RouteAddr {
    pub address: InternetAddr,
    pub protocol: Arc<str>,
}
impl Display for RouteAddr {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if let Some((_, name)) = self.reverse_tunnel() {
            return write!(f, "{}://{name}", self.protocol);
        }
        write!(f, "{}://{}", self.protocol, self.address)
    }
}
impl RouteAddr {
    pub fn udp(address: InternetAddr) -> Self {
        Self {
            address,
            protocol: Arc::from("udp"),
        }
    }
    pub fn reverse_tunnel(&self) -> Option<(ReverseTunnelTransport, &str)> {
        let transport = ReverseTunnelTransport::from_protocol(&self.protocol)?;
        let crate::addr::InternetAddrKind::DomainName { addr, port: 0 } = &*self.address else {
            return None;
        };
        Some((transport, addr))
    }
}
impl FromStr for RouteAddr {
    type Err = ParseInternetAddrError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        if let Some((protocol, address)) = s.split_once("://") {
            if ReverseTunnelTransport::from_protocol(protocol).is_some() {
                validate_reverse_tunnel_name(address)?;
                return Ok(RouteAddr {
                    protocol: protocol.into(),
                    address: InternetAddr::from_host_and_port(address, 0)?,
                });
            }
            return Ok(RouteAddr {
                protocol: protocol.into(),
                address: address.parse()?,
            });
        }
        Ok(RouteAddr::udp(s.parse()?))
    }
}

pub fn validate_reverse_tunnel_name(name: &str) -> Result<(), ParseInternetAddrError> {
    if name.is_empty()
        || name.len() > MAX_REVERSE_TUNNEL_NAME_LEN
        || !name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
    {
        return Err(ParseInternetAddrError);
    }
    Ok(())
}

#[derive(Debug, Clone, HdvSerde)]
pub struct RouteAddrHdv {
    pub addr: InternetAddrHostPort,
    pub ty: Arc<str>,
}
impl From<&RouteAddr> for RouteAddrHdv {
    fn from(value: &RouteAddr) -> Self {
        let addr = (&value.address).into();
        let ty = value.protocol.to_string().into();
        Self { addr, ty }
    }
}

#[derive(Debug, Clone)]
pub struct RouteAddrStr(pub RouteAddr);
impl Serialize for RouteAddrStr {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_str(&self.0.to_string())
    }
}
impl<'de> Deserialize<'de> for RouteAddrStr {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        deserializer.deserialize_str(RouteAddrStrVisitor)
    }
}

struct RouteAddrStrVisitor;
impl serde::de::Visitor<'_> for RouteAddrStrVisitor {
    type Value = RouteAddrStr;

    fn expecting(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
        formatter.write_str("Route address")
    }

    fn visit_str<E>(self, v: &str) -> Result<Self::Value, E>
    where
        E: serde::de::Error,
    {
        let v: RouteAddr = v.parse().map_err(|e| serde::de::Error::custom(e))?;
        Ok(RouteAddrStr(v))
    }
}

#[cfg(test)]
mod tests {
    use std::{net::SocketAddr, ops::Deref};

    use crate::addr::InternetAddrKind;

    use super::*;

    #[test]
    fn from_str_to_route_addr() {
        let addr: RouteAddr = "tcp://0.0.0.0:0".parse().unwrap();
        assert_eq!(
            addr,
            RouteAddr {
                address: "0.0.0.0:0".parse::<SocketAddr>().unwrap().into(),
                protocol: "tcp".to_string().into(),
            }
        );
    }

    #[test]
    fn reverse_tunnel_names_round_trip_without_a_fake_port() {
        for address in ["revtuntcp://private-a", "revtunrtp://private.a_1"] {
            let parsed: RouteAddr = address.parse().unwrap();
            assert_eq!(parsed.to_string(), address);
            assert_eq!(parsed.address.port(), 0);
            assert_eq!(parsed.reverse_tunnel().unwrap().1, &address[12..]);
        }
    }

    #[test]
    fn invalid_reverse_tunnel_names_are_rejected() {
        for address in [
            "revtuntcp://",
            "revtuntcp://name:1",
            "revtuntcp://name/path",
            "revtunrtp://white space",
        ] {
            assert!(address.parse::<RouteAddr>().is_err(), "{address}");
        }
        let too_long = format!(
            "revtuntcp://{}",
            "a".repeat(MAX_REVERSE_TUNNEL_NAME_LEN + 1)
        );
        assert!(too_long.parse::<RouteAddr>().is_err());
    }

    #[test]
    fn a_plain_address_defaults_to_the_udp_tag() {
        let addr: RouteAddr = "127.0.0.1:1".parse().unwrap();
        assert_eq!(
            addr,
            RouteAddr {
                address: "127.0.0.1:1".parse::<SocketAddr>().unwrap().into(),
                protocol: "udp".to_string().into(),
            }
        );
    }

    #[test]
    fn serde() {
        let s = "\"tcp://127.0.0.1:1\"";
        let v: RouteAddrStr = serde_json::from_str(s).unwrap();
        assert_eq!(
            v.0.address.deref(),
            &InternetAddrKind::SocketAddr("127.0.0.1:1".parse().unwrap())
        );
        assert_eq!(v.0.protocol, "tcp".to_string().into());
        let new_s = serde_json::to_string(&v).unwrap();
        assert_eq!(s, new_s);
    }
}
