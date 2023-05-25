use serde::{Deserialize, Serialize};

use crate::{
    crypto::XorCrypto,
    header::{ProxyConfig, RequestHeader},
};

use super::StreamAddr;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StreamType {
    Tcp,
    Kcp,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProxyConfigBuilder {
    pub address: String,
    pub stream_type: Option<StreamType>,
    pub xor_key: Vec<u8>,
}

impl ProxyConfigBuilder {
    pub fn build(self) -> StreamProxyConfig {
        ProxyConfig {
            address: StreamAddr {
                address: self.address.into(),
                stream_type: self.stream_type.unwrap_or(StreamType::Tcp),
            },
            crypto: XorCrypto::new(self.xor_key),
        }
    }
}

pub type StreamProxyConfig = ProxyConfig<StreamAddr>;
pub type StreamRequestHeader = RequestHeader<StreamAddr>;
