use common::{
    addr::InternetAddr,
    proxy_table::{ProxyTable, Tracer},
    udp::proxy_table::{UdpProxyTable, UdpWeightedProxyChainBuilder},
};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(transparent)]
pub struct UdpProxyTableBuilder {
    pub chains: Vec<UdpWeightedProxyChainBuilder>,
}

impl UdpProxyTableBuilder {
    pub fn build<T>(self, tracer: Option<T>) -> UdpProxyTable
    where
        T: Tracer<Address = InternetAddr> + Send + Sync + 'static,
    {
        let chains = self.chains.into_iter().map(|c| c.build()).collect();
        ProxyTable::new(chains, tracer).expect("Proxy chain is invalid")
    }
}
