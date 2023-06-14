use serde::{Deserialize, Serialize};

use crate::{
    addr::InternetAddr,
    config::{ProxyConfig, ProxyTable, WeightedProxyChain},
    crypto::XorCrypto,
};

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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UdpWeightedProxyChainBuilder {
    pub weight: usize,
    pub chain: Vec<UdpProxyConfigBuilder>,
}

impl UdpWeightedProxyChainBuilder {
    pub fn build(self) -> UdpWeightedProxyChain {
        WeightedProxyChain {
            weight: self.weight,
            chain: self.chain.into_iter().map(|c| c.build()).collect(),
            payload_crypto: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(transparent)]
pub struct UdpProxyTableBuilder {
    pub chains: Vec<UdpWeightedProxyChainBuilder>,
}

impl UdpProxyTableBuilder {
    pub fn build(self) -> UdpProxyTable {
        let chains = self.chains.into_iter().map(|c| c.build()).collect();
        ProxyTable::new(chains).expect("Proxy chain is invalid")
    }
}

pub type UdpProxyConfig = ProxyConfig<InternetAddr>;
pub type UdpWeightedProxyChain = WeightedProxyChain<InternetAddr>;
pub type UdpProxyTable = ProxyTable<InternetAddr>;
