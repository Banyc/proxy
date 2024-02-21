use common::proxy_table::{
    ProxyAction, ProxyConfig, ProxyConfigBuilder, ProxyGroup, ProxyGroupBuilder, ProxyTable,
    ProxyTableBuilder, ProxyTableEntry, WeightedProxyChain,
};

use super::addr::{ConcreteStreamAddr, ConcreteStreamAddrStr};

pub type StreamProxyConfigBuilder = ProxyConfigBuilder<ConcreteStreamAddrStr>;
pub type StreamProxyConfig = ProxyConfig<ConcreteStreamAddr>;
pub type StreamProxyChain = [StreamProxyConfig];
pub type StreamWeightedProxyChain = WeightedProxyChain<ConcreteStreamAddr>;
pub type StreamProxyTable = ProxyTable<ConcreteStreamAddr>;
pub type StreamProxyTableEntry = ProxyTableEntry<ConcreteStreamAddr>;
pub type StreamProxyTableEntryAction = ProxyAction<ConcreteStreamAddr>;
pub type StreamProxyGroup = ProxyGroup<ConcreteStreamAddr>;
pub type StreamProxyTableBuilder = ProxyTableBuilder<ConcreteStreamAddrStr>;
pub type StreamProxyGroupBuilder = ProxyGroupBuilder<ConcreteStreamAddrStr>;
