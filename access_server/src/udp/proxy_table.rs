use std::num::NonZeroUsize;

use common::{
    proxy_table::ProxyTable,
    udp::proxy_table::{UdpProxyTable, UdpWeightedProxyChainBuilder},
};
use proxy_client::udp::UdpTracer;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UdpProxyTableBuilder {
    pub chains: Vec<UdpWeightedProxyChainBuilder>,
    pub trace_rtt: bool,
    pub active_chains: Option<NonZeroUsize>,
}

impl UdpProxyTableBuilder {
    pub fn build(self) -> UdpProxyTable {
        let chains = self.chains.into_iter().map(|c| c.build()).collect();
        let tracer = match self.trace_rtt {
            true => Some(UdpTracer::new()),
            false => None,
        };
        ProxyTable::new(chains, tracer, self.active_chains).expect("Proxy chain is invalid")
    }
}
