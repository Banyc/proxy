use std::{fmt::Display, str::FromStr, sync::Arc};

use hdv_derive::HdvSerde;
use serde::{Deserialize, Serialize};

use crate::addr::{InternetAddr, InternetAddrHdv, ParseInternetAddrError};

/// A route address: an internet address tagged with the stream protocol used
/// to reach it. UDP routes carry the fixed `"udp"` tag.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Deserialize, Serialize)]
pub struct RouteAddr {
    pub address: InternetAddr,
    pub protocol: Arc<str>,
}
impl Display for RouteAddr {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}://{}", self.protocol, self.address)
    }
}
impl RouteAddr {
    /// Wrap an address with the fixed `"udp"` protocol tag used by UDP tables.
    pub fn udp(address: InternetAddr) -> Self {
        Self {
            address,
            protocol: Arc::from("udp"),
        }
    }
}
impl FromStr for RouteAddr {
    type Err = ParseInternetAddrError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        if let Some((protocol, address)) = s.split_once("://") {
            return Ok(RouteAddr {
                protocol: protocol.into(),
                address: address.parse()?,
            });
        }
        // No `protocol://` prefix: default to the fixed UDP tag, matching the
        // plain-address spelling of UDP route configs.
        Ok(RouteAddr::udp(s.parse()?))
    }
}

#[derive(Debug, Clone, HdvSerde)]
pub struct RouteAddrHdv {
    pub addr: InternetAddrHdv,
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
