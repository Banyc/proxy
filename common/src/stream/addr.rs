use std::{fmt::Display, str::FromStr, sync::Arc};

use serde::{Deserialize, Serialize};

use crate::addr::{InternetAddr, ParseInternetAddrError};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StreamType {
    Tcp,
    Kcp,
}

impl Display for StreamType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            StreamType::Tcp => write!(f, "tcp"),
            StreamType::Kcp => write!(f, "kcp"),
        }
    }
}

impl FromStr for StreamType {
    type Err = ParseInternetAddrError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "tcp" => Ok(Self::Tcp),
            "kcp" => Ok(Self::Kcp),
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

#[cfg(test)]
mod tests {
    use std::net::SocketAddr;

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
}
