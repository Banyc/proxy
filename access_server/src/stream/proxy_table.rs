use common::{
    proxy_table::{ProxyTable, Tracer},
    stream::{addr::StreamAddr, proxy_table::{StreamProxyTable, StreamWeightedProxyChainBuilder}},
};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(transparent)]
pub struct StreamProxyTableBuilder {
    pub chains: Vec<StreamWeightedProxyChainBuilder>,
}

impl StreamProxyTableBuilder {
    pub fn build<T>(self, tracer: Option<T>) -> StreamProxyTable
    where
        T: Tracer<Address = StreamAddr> + Send + Sync + 'static,
    {
        let chains = self.chains.into_iter().map(|c| c.build()).collect();
        ProxyTable::new(chains, tracer).expect("Proxy chain is invalid")
    }
}
