use common::{
    proxy_table::{ProxyTable},
    udp::proxy_table::{UdpProxyTable, UdpWeightedProxyChainBuilder},
};
use proxy_client::udp::UdpTracer;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UdpProxyTableBuilder {
    pub chains: Vec<UdpWeightedProxyChainBuilder>,
    pub trace_rtt: bool,
}

impl UdpProxyTableBuilder {
    pub fn build(self) -> UdpProxyTable {
        let chains = self.chains.into_iter().map(|c| c.build()).collect();
        let tracer = match self.trace_rtt {
            true => Some(UdpTracer::new()),
            false => None,
        };
        ProxyTable::new(chains, tracer).expect("Proxy chain is invalid")
    }
}
