// use std::{collections::HashMap, sync::Arc};

// use serde::{Deserialize, Serialize};
// use thiserror::Error;

// use crate::{
//     config::SharableConfig,
//     proxy_table::{
//         ProxyAction, ProxyConfig, ProxyGroup, ProxyTable, ProxyTableEntry, WeightedProxyChain,
//     },
// };

// use super::{addr::StreamAddrStr, StreamAddr};

// #[derive(Debug, Clone, Serialize, Deserialize)]
// #[serde(deny_unknown_fields)]
// pub struct StreamProxyConfigBuilder<SAS> {
//     pub address: SAS,
//     pub header_key: tokio_chacha20::config::ConfigBuilder,
//     pub payload_key: Option<tokio_chacha20::config::ConfigBuilder>,
// }

// impl<SAS> StreamProxyConfigBuilder<SAS> {
//     pub fn build<ST>(self) -> Result<StreamProxyConfig<ST>, StreamProxyConfigBuildError>
//     where
//         SAS: StreamAddrStr<StreamType = ST>,
//     {
//         let header_crypto = self.header_key.build()?;
//         let payload_crypto = self.payload_key.map(|p| p.build()).transpose()?;
//         let address = self.address.into_inner();
//         Ok(ProxyConfig {
//             address,
//             header_crypto,
//             payload_crypto,
//         })
//     }
// }

// #[derive(Debug, Error)]
// pub enum StreamProxyConfigBuildError {
//     #[error("{0}")]
//     Crypto(#[from] tokio_chacha20::config::ConfigBuildError),
//     #[error("Key not found: {0}")]
//     KeyNotFound(Arc<str>),
//     #[error("Multiple payload keys")]
//     MultiplePayloadKeys,
// }

// #[derive(Debug, Clone, Serialize, Deserialize)]
// #[serde(deny_unknown_fields)]
// pub struct StreamWeightedProxyChainBuilder<SAS> {
//     pub weight: usize,
//     pub chain: Vec<SharableConfig<StreamProxyConfigBuilder<SAS>>>,
// }

// impl<SAS> StreamWeightedProxyChainBuilder<SAS> {
//     pub fn build<ST: Clone>(
//         self,
//         proxy_server: &HashMap<Arc<str>, StreamProxyConfig<ST>>,
//     ) -> Result<StreamWeightedProxyChain<ST>, StreamProxyConfigBuildError>
//     where
//         SAS: StreamAddrStr<StreamType = ST>,
//     {
//         let chain = self
//             .chain
//             .into_iter()
//             .map(|c| match c {
//                 SharableConfig::SharingKey(k) => proxy_server
//                     .get(&k)
//                     .cloned()
//                     .ok_or_else(|| StreamProxyConfigBuildError::KeyNotFound(k)),
//                 SharableConfig::Private(c) => c.build(),
//             })
//             .collect::<Result<Arc<_>, _>>()?;
//         let mut payload_crypto = None;
//         for proxy_config in chain.iter() {
//             let Some(p) = &proxy_config.payload_crypto else {
//                 continue;
//             };
//             if payload_crypto.is_some() {
//                 return Err(StreamProxyConfigBuildError::MultiplePayloadKeys);
//             }
//             payload_crypto = Some(p.clone());
//         }
//         Ok(WeightedProxyChain {
//             weight: self.weight,
//             chain,
//             payload_crypto,
//         })
//     }
// }

// pub type StreamProxyConfig<ST> = ProxyConfig<StreamAddr<ST>>;
// pub type StreamProxyChain<ST> = [StreamProxyConfig<ST>];
// pub type StreamWeightedProxyChain<ST> = WeightedProxyChain<StreamAddr<ST>>;
// pub type StreamProxyTable<ST> = ProxyTable<StreamAddr<ST>>;
// pub type StreamProxyTableEntry<ST> = ProxyTableEntry<StreamAddr<ST>>;
// pub type StreamProxyTableEntryAction<ST> = ProxyAction<StreamAddr<ST>>;
// pub type StreamProxyGroup<ST> = ProxyGroup<StreamAddr<ST>>;
