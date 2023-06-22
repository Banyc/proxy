use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::{
    crypto::XorCrypto,
    proxy_table::{ProxyConfig, ProxyTable, Tracer, WeightedProxyChain},
};

use super::StreamAddr;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StreamProxyConfigBuilder {
    pub address: Arc<str>,
    pub xor_key: Arc<[u8]>,
}

impl StreamProxyConfigBuilder {
    pub fn build(self) -> StreamProxyConfig {
        ProxyConfig {
            address: self.address.as_ref().into(),
            crypto: XorCrypto::new(self.xor_key),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StreamWeightedProxyChainBuilder {
    pub weight: usize,
    pub chain: Vec<StreamProxyConfigBuilder>,
    pub payload_xor_key: Option<Arc<[u8]>>,
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
    pub fn build<T>(self, tracer: Option<T>) -> StreamProxyTable
    where
        T: Tracer<Address = StreamAddr> + Send + Sync + 'static,
    {
        let chains = self.chains.into_iter().map(|c| c.build()).collect();
        ProxyTable::new(chains, tracer).expect("Proxy chain is invalid")
    }
}

pub type StreamProxyConfig = ProxyConfig<StreamAddr>;
pub type StreamProxyChain = [StreamProxyConfig];
pub type StreamWeightedProxyChain = WeightedProxyChain<StreamAddr>;
pub type StreamProxyTable = ProxyTable<StreamAddr>;
