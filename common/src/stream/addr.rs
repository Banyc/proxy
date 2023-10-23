use std::{fmt::Display, str::FromStr, sync::Arc};

use serde::{de::Visitor, Deserialize, Serialize};

use crate::addr::{InternetAddr, ParseInternetAddrError};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, enum_map::Enum)]
#[serde(rename_all = "snake_case")]
pub enum StreamType {
    Tcp,
    Kcp,
    Mptcp,
}

impl Display for StreamType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            StreamType::Tcp => write!(f, "tcp"),
            StreamType::Kcp => write!(f, "kcp"),
            StreamType::Mptcp => write!(f, "mptcp"),
        }
    }
}

impl FromStr for StreamType {
    type Err = ParseInternetAddrError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "tcp" => Ok(Self::Tcp),
            "kcp" => Ok(Self::Kcp),
            "mptcp" => Ok(Self::Mptcp),
            _ => Err(ParseInternetAddrError),
        }
    }
}

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
    pub stream_type: StreamType,
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
        let stream_type: StreamType = parts.next().ok_or(ParseInternetAddrError)?.parse()?;
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

#[derive(Debug, Clone)]
pub struct StreamAddrStr(pub StreamAddr);

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

impl<'de> Visitor<'de> for StreamAddrStrVisitor {
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
                stream_type: StreamType::Tcp,
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
        assert_eq!(v.0.stream_type, StreamType::Tcp);
        let new_s = serde_json::to_string(&v).unwrap();
        assert_eq!(s, new_s);
    }
}
