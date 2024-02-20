use std::{collections::HashMap, num::NonZeroUsize, sync::Arc};

use common::{
    filter::MatcherBuilder,
    proxy_table::{ProxyTable, ProxyTableError, ProxyTableGroup},
    stream::proxy_table::{
        StreamProxyConfig, StreamProxyConfigBuildError, StreamProxyTable, StreamProxyTableGroup,
        StreamWeightedProxyChainBuilder,
    },
};
use protocol::stream::{
    addr::{ConcreteStreamAddrStr, ConcreteStreamType},
    context::ConcreteStreamContext,
};
use proxy_client::stream::StreamTracer;
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio_util::sync::CancellationToken;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StreamProxyTableBuilder {
    pub groups: Vec<StreamProxyTableGroupBuilder>,
}
impl StreamProxyTableBuilder {
    pub fn build(
        self,
        stream_proxy: &HashMap<Arc<str>, StreamProxyConfig<ConcreteStreamType>>,
        stream_context: &ConcreteStreamContext,
        cancellation: CancellationToken,
    ) -> Result<StreamProxyTable<ConcreteStreamType>, StreamProxyTableBuildError> {
        let mut built = vec![];
        for group in self.groups {
            let g = group.build(stream_proxy, stream_context, cancellation.clone())?;
            built.push(g);
        }
        Ok(ProxyTable::new(built))
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StreamProxyTableGroupBuilder {
    pub matcher: MatcherBuilder,
    pub chains: Vec<StreamWeightedProxyChainBuilder<ConcreteStreamAddrStr>>,
    pub trace_rtt: bool,
    pub active_chains: Option<NonZeroUsize>,
}
impl StreamProxyTableGroupBuilder {
    pub fn build(
        self,
        stream_proxy: &HashMap<Arc<str>, StreamProxyConfig<ConcreteStreamType>>,
        stream_context: &ConcreteStreamContext,
        cancellation: CancellationToken,
    ) -> Result<StreamProxyTableGroup<ConcreteStreamType>, StreamProxyTableBuildError> {
        let matcher = self
            .matcher
            .build()
            .map_err(StreamProxyTableBuildError::Matcher)?;
        let chains = self
            .chains
            .into_iter()
            .map(|c| c.build(stream_proxy))
            .collect::<Result<_, _>>()
            .map_err(StreamProxyTableBuildError::ChainConfig)?;
        let tracer = match self.trace_rtt {
            true => Some(StreamTracer::new(stream_context.clone())),
            false => None,
        };
        Ok(ProxyTableGroup::new(
            matcher,
            chains,
            tracer,
            self.active_chains,
            cancellation,
        )?)
    }
}

#[derive(Debug, Error)]
pub enum StreamProxyTableBuildError {
    #[error("Matcher: {0}")]
    Matcher(#[source] regex::Error),
    #[error("Chain config is invalid: {0}")]
    ChainConfig(#[source] StreamProxyConfigBuildError),
    #[error("{0}")]
    ProxyTable(#[from] ProxyTableError),
}
