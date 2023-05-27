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
pub struct StreamAddrBuilder {
    pub address: String,
    pub stream_type: StreamType,
}

impl StreamAddrBuilder {
    pub fn build(self) -> StreamAddr {
        StreamAddr {
            address: self.address.into(),
            stream_type: self.stream_type,
        }
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
