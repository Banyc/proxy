use std::{collections::HashMap, num::NonZeroUsize, sync::Arc};

use common::{
    filter::MatcherBuilder,
    proxy_table::{ProxyTable, ProxyTableError, ProxyTableGroup},
    udp::proxy_table::{
        UdpProxyConfig, UdpProxyConfigBuildError, UdpProxyTable, UdpProxyTableGroup,
        UdpWeightedProxyChainBuilder,
    },
};
use proxy_client::udp::UdpTracer;
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio_util::sync::CancellationToken;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UdpProxyTableBuilder {
    pub groups: Vec<UdpProxyTableGroupBuilder>,
}
impl UdpProxyTableBuilder {
    pub fn build(
        self,
        udp_proxy: &HashMap<Arc<str>, UdpProxyConfig>,
        cancellation: CancellationToken,
    ) -> Result<UdpProxyTable, UdpProxyTableBuildError> {
        let mut built = vec![];
        for group in self.groups {
            let g = group.build(udp_proxy, cancellation.clone())?;
            built.push(g);
        }
        Ok(ProxyTable::new(built))
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UdpProxyTableGroupBuilder {
    pub matcher: MatcherBuilder,
    pub chains: Vec<UdpWeightedProxyChainBuilder>,
    pub trace_rtt: bool,
    pub active_chains: Option<NonZeroUsize>,
}
impl UdpProxyTableGroupBuilder {
    pub fn build(
        self,
        udp_proxy: &HashMap<Arc<str>, UdpProxyConfig>,
        cancellation: CancellationToken,
    ) -> Result<UdpProxyTableGroup, UdpProxyTableBuildError> {
        let matcher = self
            .matcher
            .build()
            .map_err(UdpProxyTableBuildError::Matcher)?;
        let chains = self
            .chains
            .into_iter()
            .map(|c| c.build(udp_proxy))
            .collect::<Result<_, _>>()
            .map_err(UdpProxyTableBuildError::ChainConfig)?;
        let tracer = match self.trace_rtt {
            true => Some(UdpTracer::new()),
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
pub enum UdpProxyTableBuildError {
    #[error("Matcher: {0}")]
    Matcher(#[source] regex::Error),
    #[error("Chain config is invalid: {0}")]
    ChainConfig(#[source] UdpProxyConfigBuildError),
    #[error("{0}")]
    ProxyTable(#[from] ProxyTableError),
}
