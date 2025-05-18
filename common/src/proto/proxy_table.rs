use crate::{
    addr::{InternetAddr, InternetAddrStr},
    proxy_table::{
        ProxyAction, ProxyConfig, ProxyConfigBuilder, ProxyGroup, ProxyGroupBuilder, ProxyTable,
        ProxyTableBuilder, ProxyTableEntry, WeightedProxyChain,
    },
};

use super::addr::{StreamAddr, StreamAddrStr};

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

pub type UdpProxyConfigBuilder = ProxyConfigBuilder<InternetAddrStr>;
pub type UdpProxyConfig = ProxyConfig<InternetAddr>;
pub type UdpProxyChain = [UdpProxyConfig];
pub type UdpWeightedProxyChain = WeightedProxyChain<InternetAddr>;
pub type UdpProxyTable = ProxyTable<InternetAddr>;
pub type UdpProxyTableEntry = ProxyTableEntry<InternetAddr>;
pub type UdpProxyTableEntryAction = ProxyAction<InternetAddr>;
pub type UdpProxyGroup = ProxyGroup<InternetAddr>;
pub type UdpProxyTableBuilder = ProxyTableBuilder<InternetAddrStr>;
pub type UdpProxyGroupBuilder = ProxyGroupBuilder<InternetAddrStr>;
