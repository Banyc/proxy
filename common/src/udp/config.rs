use serde::{Deserialize, Serialize};

use crate::{addr::InternetAddr, config::ProxyConfig, crypto::XorCrypto};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UdpProxyConfigBuilder {
    pub address: String,
    pub xor_key: Vec<u8>,
}

impl UdpProxyConfigBuilder {
    pub fn build(self) -> UdpProxyConfig {
        ProxyConfig {
            address: self.address.into(),
            crypto: XorCrypto::new(self.xor_key),
        }
    }
}

pub type UdpProxyConfig = ProxyConfig<InternetAddr>;
