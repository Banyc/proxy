use std::{fmt, str::FromStr};

use serde::{de::Visitor, Deserialize, Serialize};

use common::{
    addr::ParseInternetAddrError,
    proxy_table::AddressString,
    stream::addr::{StreamAddr, StreamType},
};

pub type ConcreteStreamAddr = StreamAddr<ConcreteStreamType>;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConcreteStreamType {
    Tcp,
    TcpMux,
    Kcp,
    Mptcp,
    Rtp,
    RtpMux,
}
impl StreamType for ConcreteStreamType {}
impl fmt::Display for ConcreteStreamType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ConcreteStreamType::Tcp => write!(f, "tcp"),
            ConcreteStreamType::TcpMux => write!(f, "tcpmux"),
            ConcreteStreamType::Kcp => write!(f, "kcp"),
            ConcreteStreamType::Mptcp => write!(f, "mptcp"),
            ConcreteStreamType::Rtp => write!(f, "rtp"),
            ConcreteStreamType::RtpMux => write!(f, "rtpmux"),
        }
    }
}
impl FromStr for ConcreteStreamType {
    type Err = ParseInternetAddrError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "tcp" => Ok(Self::Tcp),
            "tcpmux" => Ok(Self::TcpMux),
            "kcp" => Ok(Self::Kcp),
            "mptcp" => Ok(Self::Mptcp),
            "rtp" => Ok(Self::Rtp),
            "rtpmux" => Ok(Self::RtpMux),
            _ => Err(ParseInternetAddrError),
        }
    }
}

#[derive(Debug, Clone)]
pub struct ConcreteStreamAddrStr(pub ConcreteStreamAddr);
impl AddressString for ConcreteStreamAddrStr {
    type Address = ConcreteStreamAddr;

    fn into_address(self) -> Self::Address {
        self.0
    }
}
impl Serialize for ConcreteStreamAddrStr {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_str(&self.0.to_string())
    }
}
impl<'de> Deserialize<'de> for ConcreteStreamAddrStr {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        deserializer.deserialize_str(ConcreteStreamAddrStrVisitor)
    }
}

struct ConcreteStreamAddrStrVisitor;
impl Visitor<'_> for ConcreteStreamAddrStrVisitor {
    type Value = ConcreteStreamAddrStr;

    fn expecting(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
        formatter.write_str("Stream address")
    }

    fn visit_str<E>(self, v: &str) -> Result<Self::Value, E>
    where
        E: serde::de::Error,
    {
        let v: ConcreteStreamAddr = v.parse().map_err(|e| serde::de::Error::custom(e))?;
        Ok(ConcreteStreamAddrStr(v))
    }
}

#[cfg(test)]
mod tests {
    use std::{net::SocketAddr, ops::Deref};

    use common::addr::InternetAddrKind;

    use super::*;

    #[test]
    fn from_str_to_stream_addr() {
        let addr: StreamAddr<ConcreteStreamType> = "tcp://0.0.0.0:0".parse().unwrap();
        assert_eq!(
            addr,
            StreamAddr {
                address: "0.0.0.0:0".parse::<SocketAddr>().unwrap().into(),
                stream_type: ConcreteStreamType::Tcp,
            }
        );
    }

    #[test]
    fn serde() {
        let s = "\"tcp://127.0.0.1:1\"";
        let v: ConcreteStreamAddrStr = serde_json::from_str(s).unwrap();
        assert_eq!(
            v.0.address.deref(),
            &InternetAddrKind::SocketAddr("127.0.0.1:1".parse().unwrap())
        );
        assert_eq!(v.0.stream_type, ConcreteStreamType::Tcp);
        let new_s = serde_json::to_string(&v).unwrap();
        assert_eq!(s, new_s);
    }
}
