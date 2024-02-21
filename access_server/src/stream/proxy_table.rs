use common::proxy_table::{ProxyGroupBuildContext, ProxyTableBuildContext};
use protocol::stream::addr::ConcreteStreamAddr;
use proxy_client::stream::StreamTracerBuilder;

pub type StreamProxyTableBuildContext<'caller> =
    ProxyTableBuildContext<'caller, ConcreteStreamAddr, StreamTracerBuilder>;
pub type StreamProxyGroupBuildContext<'caller> =
    ProxyGroupBuildContext<'caller, ConcreteStreamAddr, StreamTracerBuilder>;

// use std::{collections::HashMap, num::NonZeroUsize, sync::Arc};

// use common::{
//     config::SharableConfig,
//     filter::{Matcher, MatcherBuilder},
//     proxy_table::{ProxyAction, ProxyGroup, ProxyTable, ProxyTableEntry, ProxyTableError},
//     stream::proxy_table::{
//         StreamProxyConfig, StreamProxyConfigBuildError, StreamProxyGroup, StreamProxyTable,
//         StreamProxyTableEntry, StreamProxyTableEntryAction, StreamWeightedProxyChainBuilder,
//     },
// };
// use protocol::stream::{
//     addr::{ConcreteStreamAddrStr, ConcreteStreamType},
//     context::ConcreteStreamContext,
// };
// use proxy_client::stream::StreamTracer;
// use serde::{Deserialize, Serialize};
// use thiserror::Error;
// use tokio_util::sync::CancellationToken;

// #[derive(Debug, Clone, Serialize, Deserialize)]
// #[serde(deny_unknown_fields)]
// pub struct StreamProxyTableBuilder {
//     pub entries: Vec<StreamProxyTableEntryBuilder>,
// }
// impl StreamProxyTableBuilder {
//     pub fn build(
//         self,
//         cx: StreamProxyTableBuildContext<'_>,
//     ) -> Result<StreamProxyTable<ConcreteStreamType>, StreamProxyTableBuildError> {
//         let mut built = vec![];
//         for entry in self.entries {
//             let e = entry.build(cx.clone())?;
//             built.push(e);
//         }
//         Ok(ProxyTable::new(built))
//     }
// }
// #[derive(Debug, Clone)]
// pub struct StreamProxyTableBuildContext<'caller> {
//     pub matcher: &'caller HashMap<Arc<str>, Matcher>,
//     pub stream_proxy_group: &'caller HashMap<Arc<str>, StreamProxyGroup<ConcreteStreamType>>,
//     pub proxy_group_cx: StreamProxyGroupBuildContext<'caller>,
// }

// #[derive(Debug, Clone, Serialize, Deserialize)]
// #[serde(deny_unknown_fields)]
// pub struct StreamProxyTableEntryBuilder {
//     matcher: SharableConfig<MatcherBuilder>,
//     #[serde(flatten)]
//     action: StreamProxyActionBuilder,
// }
// impl StreamProxyTableEntryBuilder {
//     pub fn build(
//         self,
//         cx: StreamProxyTableBuildContext<'_>,
//     ) -> Result<StreamProxyTableEntry<ConcreteStreamType>, StreamProxyTableBuildError> {
//         let matcher = match self.matcher {
//             SharableConfig::SharingKey(k) => cx
//                 .matcher
//                 .get(&k)
//                 .cloned()
//                 .ok_or_else(|| StreamProxyTableBuildError::KeyNotFound(k))?,
//             SharableConfig::Private(v) => v.build().map_err(StreamProxyTableBuildError::Matcher)?,
//         };
//         let action = self
//             .action
//             .build(cx.stream_proxy_group, cx.proxy_group_cx)?;
//         Ok(ProxyTableEntry::new(matcher, action))
//     }
// }

// #[derive(Debug, Clone, Serialize, Deserialize)]
// #[serde(deny_unknown_fields)]
// #[serde(rename = "snake_case")]
// pub enum StreamProxyActionBuilder {
//     Direct,
//     Block,
//     ProxyGroup(SharableConfig<StreamProxyGroupBuilder>),
// }
// impl StreamProxyActionBuilder {
//     pub fn build(
//         self,
//         stream_proxy_group: &HashMap<Arc<str>, StreamProxyGroup<ConcreteStreamType>>,
//         proxy_group_cx: StreamProxyGroupBuildContext<'_>,
//     ) -> Result<StreamProxyTableEntryAction<ConcreteStreamType>, StreamProxyTableBuildError> {
//         Ok(match self {
//             StreamProxyActionBuilder::Direct => ProxyAction::Direct,
//             StreamProxyActionBuilder::Block => ProxyAction::Block,
//             StreamProxyActionBuilder::ProxyGroup(p) => ProxyAction::ProxyGroup(Arc::new(match p {
//                 SharableConfig::SharingKey(k) => stream_proxy_group
//                     .get(&k)
//                     .cloned()
//                     .ok_or_else(|| StreamProxyTableBuildError::KeyNotFound(k))?,
//                 SharableConfig::Private(p) => p.build(proxy_group_cx)?,
//             })),
//         })
//     }
// }

// #[derive(Debug, Clone, Serialize, Deserialize)]
// #[serde(deny_unknown_fields)]
// pub struct StreamProxyGroupBuilder {
//     pub chains: Vec<StreamWeightedProxyChainBuilder<ConcreteStreamAddrStr>>,
//     pub trace_rtt: bool,
//     pub active_chains: Option<NonZeroUsize>,
// }
// impl StreamProxyGroupBuilder {
//     pub fn build(
//         self,
//         cx: StreamProxyGroupBuildContext<'_>,
//     ) -> Result<StreamProxyGroup<ConcreteStreamType>, StreamProxyTableBuildError> {
//         let chains = self
//             .chains
//             .into_iter()
//             .map(|c| c.build(cx.stream_proxy_server))
//             .collect::<Result<_, _>>()
//             .map_err(StreamProxyTableBuildError::ChainConfig)?;
//         let tracer = match self.trace_rtt {
//             true => Some(StreamTracer::new(cx.stream_context.clone())),
//             false => None,
//         };
//         Ok(ProxyGroup::new(
//             chains,
//             tracer,
//             self.active_chains,
//             cx.cancellation,
//         )?)
//     }
// }
// #[derive(Debug, Clone)]
// pub struct StreamProxyGroupBuildContext<'caller> {
//     pub stream_proxy_server: &'caller HashMap<Arc<str>, StreamProxyConfig<ConcreteStreamType>>,
//     pub stream_context: &'caller ConcreteStreamContext,
//     pub cancellation: CancellationToken,
// }

// #[derive(Debug, Error)]
// pub enum StreamProxyTableBuildError {
//     #[error("Key not found: `{0}`")]
//     KeyNotFound(Arc<str>),
//     #[error("Matcher: {0}")]
//     Matcher(#[source] regex::Error),
//     #[error("Chain config is invalid: {0}")]
//     ChainConfig(#[source] StreamProxyConfigBuildError),
//     #[error("{0}")]
//     ProxyTable(#[from] ProxyTableError),
// }
