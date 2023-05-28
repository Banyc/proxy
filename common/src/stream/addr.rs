use std::fmt::Display;

use serde::{Deserialize, Serialize};

use crate::addr::InternetAddr;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StreamType {
    Tcp,
    Kcp,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Deserialize, Serialize)]
#[serde(transparent)]
pub struct StreamAddrBuilder {
    pub address: String,
}

impl StreamAddrBuilder {
    pub fn build(self) -> StreamAddr {
        self.address.as_str().into()
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
        match self.stream_type {
            StreamType::Tcp => write!(f, "tcp://{}", self.address),
            StreamType::Kcp => write!(f, "kcp://{}", self.address),
        }
    }
}

impl From<&str> for StreamAddr {
    fn from(s: &str) -> Self {
        let mut parts = s.split("://");
        let stream_type = match parts.next() {
            Some("tcp") => StreamType::Tcp,
            Some("kcp") => StreamType::Kcp,
            _ => panic!("invalid stream address"),
        };
        let address: String = parts.next().unwrap().parse().unwrap();
        assert!(parts.next().is_none());
        StreamAddr {
            address: address.into(),
            stream_type,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::net::SocketAddr;

    use super::*;

    #[test]
    fn from_str_to_stream_addr() {
        let addr: StreamAddr = "tcp://0.0.0.0:0".into();
        assert_eq!(
            addr,
            StreamAddr {
                address: "0.0.0.0:0".parse::<SocketAddr>().unwrap().into(),
                stream_type: StreamType::Tcp,
            }
        );
    }
}
