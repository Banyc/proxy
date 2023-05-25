use serde::{Deserialize, Serialize};

use crate::{
    crypto::XorCrypto,
    header::{InternetAddr, ProxyConfig},
};

/// A stream address
#[derive(Debug, Clone, PartialEq, Eq, Hash, Deserialize, Serialize)]
pub struct RequestHeader {
    pub address: InternetAddr,
    pub stream_type: StreamType,
}

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
    pub fn build(self) -> ProxyConfig<RequestHeader> {
        ProxyConfig {
            header: RequestHeader {
                address: self.address.into(),
                stream_type: self.stream_type.unwrap_or(StreamType::Tcp),
            },
            crypto: XorCrypto::new(self.xor_key),
        }
    }
}

pub type StreamProxyConfig = ProxyConfig<RequestHeader>;
