use common::{
    proxy_table::{
        ProxyAction, ProxyConfig, ProxyConfigBuilder, ProxyGroup, ProxyGroupBuilder, ProxyTable,
        ProxyTableBuilder, ProxyTableEntry, WeightedProxyChain,
    },
    stream::addr::{StreamAddr, StreamAddrStr},
};

pub type StreamProxyConfigBuilder = ProxyConfigBuilder<StreamAddrStr>;
pub type StreamProxyConfig = ProxyConfig<StreamAddr>;
pub type StreamProxyChain = [StreamProxyConfig];
pub type StreamWeightedProxyChain = WeightedProxyChain<StreamAddr>;
pub type StreamProxyTable = ProxyTable<StreamAddr>;
pub type StreamProxyTableEntry = ProxyTableEntry<StreamAddr>;
pub type StreamProxyTableEntryAction = ProxyAction<StreamAddr>;
pub type StreamProxyGroup = ProxyGroup<StreamAddr>;
pub type StreamProxyTableBuilder = ProxyTableBuilder<StreamAddrStr>;
pub type StreamProxyGroupBuilder = ProxyGroupBuilder<StreamAddrStr>;
