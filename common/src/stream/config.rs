use serde::{Deserialize, Serialize};

use crate::{config::ProxyConfig, crypto::XorCrypto};

use super::StreamAddr;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StreamProxyConfigBuilder {
    pub address: String,
    pub xor_key: Vec<u8>,
}

impl StreamProxyConfigBuilder {
    pub fn build(self) -> StreamProxyConfig {
        ProxyConfig {
            address: self.address.as_str().into(),
            crypto: XorCrypto::new(self.xor_key),
        }
    }
}

pub type StreamProxyConfig = ProxyConfig<StreamAddr>;
