use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::{
    addr::InternetAddr,
    crypto::XorCrypto,
    proxy_table::{ProxyConfig, ProxyTable, WeightedProxyChain},
};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UdpProxyConfigBuilder {
    pub address: Arc<str>,
    pub xor_key: Arc<[u8]>,
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
