use std::num::NonZeroUsize;

use common::{
    proxy_table::{ProxyTable, ProxyTableError},
    stream::{
        pool::Pool,
        proxy_table::{
            StreamProxyConfigBuildError, StreamProxyTable, StreamWeightedProxyChainBuilder,
        },
    },
};
use proxy_client::stream::StreamTracer;
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio_util::sync::CancellationToken;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StreamProxyTableBuilder {
    pub chains: Vec<StreamWeightedProxyChainBuilder>,
    pub trace_rtt: bool,
    pub active_chains: Option<NonZeroUsize>,
}

impl StreamProxyTableBuilder {
    pub fn build(
        self,
        stream_pool: &Pool,
        cancellation: CancellationToken,
    ) -> Result<StreamProxyTable, StreamProxyTableBuildError> {
        let chains = self
            .chains
            .into_iter()
            .map(|c| c.build())
            .collect::<Result<_, _>>()
            .map_err(StreamProxyTableBuildError::ChainConfig)?;
        let tracer = match self.trace_rtt {
            true => Some(StreamTracer::new(stream_pool.clone())),
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
