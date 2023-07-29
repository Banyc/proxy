use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::{
    crypto::{XorCryptoBuildError, XorCryptoBuilder},
    proxy_table::{ProxyConfig, ProxyTable, WeightedProxyChain},
};

use super::StreamAddr;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StreamProxyConfigBuilder {
    pub address: Arc<str>,
    pub xor_key: XorCryptoBuilder,
}

impl StreamProxyConfigBuilder {
    pub fn build(self) -> Result<StreamProxyConfig, XorCryptoBuildError> {
        let crypto = self.xor_key.build()?;
        Ok(ProxyConfig {
            address: self.address.as_ref().into(),
            crypto,
        })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StreamWeightedProxyChainBuilder {
    pub weight: usize,
    pub chain: Vec<StreamProxyConfigBuilder>,
    pub payload_xor_key: Option<XorCryptoBuilder>,
}

impl StreamWeightedProxyChainBuilder {
    pub fn build(self) -> Result<StreamWeightedProxyChain, XorCryptoBuildError> {
        let payload_crypto = match self.payload_xor_key {
            Some(c) => Some(c.build()?),
            None => None,
        };
        let chain = self
            .chain
            .into_iter()
            .map(|c| c.build())
            .collect::<Result<_, _>>()?;
        Ok(WeightedProxyChain {
            weight: self.weight,
            chain,
            payload_crypto,
        })
    }
}

pub type StreamProxyConfig = ProxyConfig<StreamAddr>;
pub type StreamProxyChain = [StreamProxyConfig];
pub type StreamWeightedProxyChain = WeightedProxyChain<StreamAddr>;
pub type StreamProxyTable = ProxyTable<StreamAddr>;
