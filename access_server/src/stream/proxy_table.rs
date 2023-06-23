use common::{
    proxy_table::{ProxyTable},
    stream::{
        pool::Pool,
        proxy_table::{StreamProxyTable, StreamWeightedProxyChainBuilder},
    },
};
use proxy_client::stream::StreamTracer;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StreamProxyTableBuilder {
    pub chains: Vec<StreamWeightedProxyChainBuilder>,
    pub trace_rtt: bool,
}

impl StreamProxyTableBuilder {
    pub fn build(self, stream_pool: &Pool) -> StreamProxyTable {
        let chains = self.chains.into_iter().map(|c| c.build()).collect();
        let tracer = match self.trace_rtt {
            true => Some(StreamTracer::new(stream_pool.clone())),
            false => None,
        };
        ProxyTable::new(chains, tracer).expect("Proxy chain is invalid")
    }
}
