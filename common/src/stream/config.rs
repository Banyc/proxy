use serde::{Deserialize, Serialize};

use crate::{
    config::{ProxyConfig, ProxyTable, WeightedProxyChain},
    crypto::XorCrypto,
};

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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StreamWeightedProxyChainBuilder {
    pub weight: usize,
    pub chain: Vec<StreamProxyConfigBuilder>,
    pub payload_xor_key: Option<Vec<u8>>,
}

impl StreamWeightedProxyChainBuilder {
    pub fn build(self) -> StreamWeightedProxyChain {
        WeightedProxyChain {
            weight: self.weight,
            chain: self.chain.into_iter().map(|c| c.build()).collect(),
            payload_crypto: self.payload_xor_key.map(XorCrypto::new),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(transparent)]
pub struct StreamProxyTableBuilder {
    pub chains: Vec<StreamWeightedProxyChainBuilder>,
}

impl StreamProxyTableBuilder {
    pub fn build(self) -> StreamProxyTable {
        let chains = self.chains.into_iter().map(|c| c.build()).collect();
        ProxyTable::new(chains).expect("Proxy chain is invalid")
    }
}

pub type StreamProxyConfig = ProxyConfig<StreamAddr>;
pub type StreamWeightedProxyChain = WeightedProxyChain<StreamAddr>;
pub type StreamProxyTable = ProxyTable<StreamAddr>;
