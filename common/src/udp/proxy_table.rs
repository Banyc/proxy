use std::sync::Arc;

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::{
    addr::InternetAddr,
    crypto::{XorCryptoBuildError, XorCryptoBuilder},
    proxy_table::{ProxyConfig, ProxyTable, WeightedProxyChain},
};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UdpProxyConfigBuilder {
    pub address: Arc<str>,
    pub xor_key: XorCryptoBuilder,
}

impl UdpProxyConfigBuilder {
    pub fn build(self) -> Result<UdpProxyConfig, UdpProxyConfigBuildError> {
        let crypto = self.xor_key.build()?;
        let address = self
            .address
            .parse()
            .map_err(|_| UdpProxyConfigBuildError::Address)?;
        Ok(ProxyConfig { address, crypto })
    }
}

#[derive(Debug, Error)]
pub enum UdpProxyConfigBuildError {
    #[error("Address error")]
    Address,
    #[error("{0}")]
    XorCrypto(#[from] XorCryptoBuildError),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UdpWeightedProxyChainBuilder {
    pub weight: usize,
    pub chain: Vec<UdpProxyConfigBuilder>,
    pub payload_xor_key: Option<XorCryptoBuilder>,
}

impl UdpWeightedProxyChainBuilder {
    pub fn build(self) -> Result<UdpWeightedProxyChain, UdpProxyConfigBuildError> {
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

pub type UdpProxyConfig = ProxyConfig<InternetAddr>;
pub type UdpProxyChain = [UdpProxyConfig];
pub type UdpWeightedProxyChain = WeightedProxyChain<InternetAddr>;
pub type UdpProxyTable = ProxyTable<InternetAddr>;
