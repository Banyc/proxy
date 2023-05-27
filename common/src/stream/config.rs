use serde::{Deserialize, Serialize};

use crate::{config::ProxyConfig, crypto::XorCrypto};

use super::{StreamAddr, StreamType};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StreamProxyConfigBuilder {
    pub address: String,
    pub stream_type: Option<StreamType>,
    pub xor_key: Vec<u8>,
}

impl StreamProxyConfigBuilder {
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
