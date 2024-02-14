use std::{collections::HashMap, num::NonZeroUsize, sync::Arc};

use common::{
    proxy_table::{ProxyTable, ProxyTableError},
    stream::proxy_table::{
        StreamProxyConfig, StreamProxyConfigBuildError, StreamProxyTable,
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
    pub chains: Vec<StreamWeightedProxyChainBuilder<ConcreteStreamAddrStr>>,
    pub trace_rtt: bool,
    pub active_chains: Option<NonZeroUsize>,
}

impl StreamProxyTableBuilder {
    pub fn build(
        self,
        stream_proxy: &HashMap<Arc<str>, StreamProxyConfig<ConcreteStreamType>>,
        stream_context: &ConcreteStreamContext,
        cancellation: CancellationToken,
    ) -> Result<StreamProxyTable<ConcreteStreamType>, StreamProxyTableBuildError> {
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
        Ok(ProxyTable::new(
            chains,
            tracer,
            self.active_chains,
            cancellation,
        )?)
    }
}

#[derive(Debug, Error)]
pub enum StreamProxyTableBuildError {
    #[error("Chain config is invalid: {0}")]
    ChainConfig(#[source] StreamProxyConfigBuildError),
    #[error("{0}")]
    ProxyTable(#[from] ProxyTableError),
}
