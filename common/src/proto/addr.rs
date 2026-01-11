use std::{fmt::Display, str::FromStr, sync::Arc};

use hdv_derive::HdvSerde;
use serde::{Deserialize, Serialize};

use crate::{
    addr::{InternetAddr, InternetAddrHdv, ParseInternetAddrError},
    route::IntoAddr,
};

#[derive(Debug, Clone, PartialEq, Eq, Hash, Deserialize, Serialize)]
#[serde(transparent)]
pub struct StreamAddrBuilder {
    pub address: Arc<str>,
}
impl StreamAddrBuilder {
    pub fn build(self) -> Result<StreamAddr, ParseInternetAddrError> {
        self.address.as_ref().parse()
    }
}

/// A stream address
#[derive(Debug, Clone, PartialEq, Eq, Hash, Deserialize, Serialize)]
pub struct StreamAddr {
    pub address: InternetAddr,
    pub stream_type: Arc<str>,
}
impl Display for StreamAddr {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}://{}", self.stream_type, self.address)
    }
}
impl FromStr for StreamAddr {
    type Err = ParseInternetAddrError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let mut parts = s.split("://");
        let stream_type = parts.next().ok_or(ParseInternetAddrError)?.into();
        let address = parts.next().ok_or(ParseInternetAddrError)?.parse()?;
        if parts.next().is_some() {
            return Err(ParseInternetAddrError);
        }
        Ok(StreamAddr {
            address,
            stream_type,
        })
    }
}

#[derive(Debug, Clone, HdvSerde)]
pub struct StreamAddrHdv {
    pub addr: InternetAddrHdv,
    pub ty: Arc<str>,
}
impl From<&StreamAddr> for StreamAddrHdv {
    fn from(value: &StreamAddr) -> Self {
        let addr = (&value.address).into();
        let ty = value.stream_type.to_string().into();
        Self { addr, ty }
    }
}

#[derive(Debug, Clone)]
pub struct StreamAddrStr(pub StreamAddr);
impl IntoAddr for StreamAddrStr {
    type Addr = StreamAddr;
    fn into_address(self) -> Self::Addr {
        self.0
    }
}
impl Serialize for StreamAddrStr {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_str(&self.0.to_string())
    }
}
impl<'de> Deserialize<'de> for StreamAddrStr {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        deserializer.deserialize_str(StreamAddrStrVisitor)
    }
}

struct StreamAddrStrVisitor;
impl serde::de::Visitor<'_> for StreamAddrStrVisitor {
    type Value = StreamAddrStr;

    fn expecting(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
        formatter.write_str("Stream address")
    }

    fn visit_str<E>(self, v: &str) -> Result<Self::Value, E>
    where
        E: serde::de::Error,
    {
        let v: StreamAddr = v.parse().map_err(|e| serde::de::Error::custom(e))?;
        Ok(StreamAddrStr(v))
    }
}

#[cfg(test)]
mod tests {
    use std::{net::SocketAddr, ops::Deref};

    use crate::addr::InternetAddrKind;

    use super::*;

    #[test]
    fn from_str_to_stream_addr() {
        let addr: StreamAddr = "tcp://0.0.0.0:0".parse().unwrap();
        assert_eq!(
            addr,
            StreamAddr {
                address: "0.0.0.0:0".parse::<SocketAddr>().unwrap().into(),
                stream_type: "tcp".to_string().into(),
            }
        );
    }

    #[test]
    fn serde() {
        let s = "\"tcp://127.0.0.1:1\"";
        let v: StreamAddrStr = serde_json::from_str(s).unwrap();
        assert_eq!(
            v.0.address.deref(),
            &InternetAddrKind::SocketAddr("127.0.0.1:1".parse().unwrap())
        );
        assert_eq!(v.0.stream_type, "tcp".to_string().into());
        let new_s = serde_json::to_string(&v).unwrap();
        assert_eq!(s, new_s);
    }
}
