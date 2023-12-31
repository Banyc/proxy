use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::{
    addr::{InternetAddr, InternetAddrStr},
    proxy_table::{ProxyConfig, ProxyTable, WeightedProxyChain},
};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UdpProxyConfigBuilder {
    pub address: InternetAddrStr,
    pub header_key: tokio_chacha20::config::ConfigBuilder,
}

impl UdpProxyConfigBuilder {
    pub fn build(self) -> Result<UdpProxyConfig, UdpProxyConfigBuildError> {
        let crypto = self.header_key.build()?;
        let address = self.address.0;
        Ok(ProxyConfig { address, crypto })
    }
}

#[derive(Debug, Error)]
pub enum UdpProxyConfigBuildError {
    #[error("{0}")]
    Crypto(#[from] tokio_chacha20::config::ConfigBuildError),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UdpWeightedProxyChainBuilder {
    pub weight: usize,
    pub chain: Vec<UdpProxyConfigBuilder>,
    pub payload_key: Option<tokio_chacha20::config::ConfigBuilder>,
}

impl UdpWeightedProxyChainBuilder {
    pub fn build(self) -> Result<UdpWeightedProxyChain, UdpProxyConfigBuildError> {
        let payload_crypto = match self.payload_key {
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
