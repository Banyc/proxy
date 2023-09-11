use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::{
    crypto::{XorCryptoBuildError, XorCryptoBuilder},
    proxy_table::{ProxyConfig, ProxyTable, WeightedProxyChain},
};

use super::{addr::StreamAddrStr, StreamAddr};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StreamProxyConfigBuilder {
    pub address: StreamAddrStr,
    pub xor_key: XorCryptoBuilder,
}

impl StreamProxyConfigBuilder {
    pub fn build(self) -> Result<StreamProxyConfig, StreamProxyConfigBuildError> {
        let crypto = self.xor_key.build()?;
        let address = self.address.0;
        Ok(ProxyConfig { address, crypto })
    }
}

#[derive(Debug, Error)]
pub enum StreamProxyConfigBuildError {
    #[error("{0}")]
    XorCrypto(#[from] XorCryptoBuildError),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StreamWeightedProxyChainBuilder {
    pub weight: usize,
    pub chain: Vec<StreamProxyConfigBuilder>,
    pub payload_xor_key: Option<XorCryptoBuilder>,
}

impl StreamWeightedProxyChainBuilder {
    pub fn build(self) -> Result<StreamWeightedProxyChain, StreamProxyConfigBuildError> {
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
