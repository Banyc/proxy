use std::{collections::HashMap, sync::Arc};

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::{
    config::SharableConfig,
    proxy_table::{ProxyConfig, ProxyTable, WeightedProxyChain},
};

use super::{addr::StreamAddrStr, StreamAddr};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StreamProxyConfigBuilder<SAS> {
    pub address: SAS,
    pub header_key: tokio_chacha20::config::ConfigBuilder,
}

impl<SAS> StreamProxyConfigBuilder<SAS> {
    pub fn build<ST>(self) -> Result<StreamProxyConfig<ST>, StreamProxyConfigBuildError>
    where
        SAS: StreamAddrStr<StreamType = ST>,
    {
        let crypto = self.header_key.build()?;
        let address = self.address.into_inner();
        Ok(ProxyConfig { address, crypto })
    }
}

#[derive(Debug, Error)]
pub enum StreamProxyConfigBuildError {
    #[error("{0}")]
    Crypto(#[from] tokio_chacha20::config::ConfigBuildError),
    #[error("Key not found: {0}")]
    KeyNotFound(Arc<str>),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StreamWeightedProxyChainBuilder<SAS> {
    pub weight: usize,
    pub chain: Vec<SharableConfig<StreamProxyConfigBuilder<SAS>>>,
    pub payload_key: Option<tokio_chacha20::config::ConfigBuilder>,
}

impl<SAS> StreamWeightedProxyChainBuilder<SAS> {
    pub fn build<ST: Clone>(
        self,
        stream_proxy: &HashMap<Arc<str>, StreamProxyConfig<ST>>,
    ) -> Result<StreamWeightedProxyChain<ST>, StreamProxyConfigBuildError>
    where
        SAS: StreamAddrStr<StreamType = ST>,
    {
        let payload_crypto = match self.payload_key {
            Some(c) => Some(c.build()?),
            None => None,
        };
        let chain = self
            .chain
            .into_iter()
            .map(|c| match c {
                SharableConfig::SharingKey(k) => stream_proxy
                    .get(&k)
                    .cloned()
                    .ok_or_else(|| StreamProxyConfigBuildError::KeyNotFound(k)),
                SharableConfig::Private(c) => c.build(),
            })
            .collect::<Result<_, _>>()?;
        Ok(WeightedProxyChain {
            weight: self.weight,
            chain,
            payload_crypto,
        })
    }
}

pub type StreamProxyConfig<ST> = ProxyConfig<StreamAddr<ST>>;
pub type StreamProxyChain<ST> = [StreamProxyConfig<ST>];
pub type StreamWeightedProxyChain<ST> = WeightedProxyChain<StreamAddr<ST>>;
pub type StreamProxyTable<ST> = ProxyTable<StreamAddr<ST>>;
