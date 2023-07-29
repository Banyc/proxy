use std::num::NonZeroUsize;

use common::{
    crypto::XorCryptoBuildError,
    proxy_table::{ProxyTable, ProxyTableError},
    udp::proxy_table::{UdpProxyTable, UdpWeightedProxyChainBuilder},
};
use proxy_client::udp::UdpTracer;
use serde::{Deserialize, Serialize};
use thiserror::Error;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UdpProxyTableBuilder {
    pub chains: Vec<UdpWeightedProxyChainBuilder>,
    pub trace_rtt: bool,
    pub active_chains: Option<NonZeroUsize>,
}

impl UdpProxyTableBuilder {
    pub fn build(self) -> Result<UdpProxyTable, UdpProxyTableBuildError> {
        let chains = self
            .chains
            .into_iter()
            .map(|c| c.build())
            .collect::<Result<_, _>>()
            .map_err(UdpProxyTableBuildError::ChainConfig)?;
        let tracer = match self.trace_rtt {
            true => Some(UdpTracer::new()),
            false => None,
        };
        Ok(ProxyTable::new(chains, tracer, self.active_chains)?)
    }
}

#[derive(Debug, Error)]
pub enum UdpProxyTableBuildError {
    #[error("Chain config is invalid: {0}")]
    ChainConfig(#[source] XorCryptoBuildError),
    #[error("{0}")]
    ProxyTable(#[from] ProxyTableError),
}
