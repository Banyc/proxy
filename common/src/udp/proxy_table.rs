use crate::{
    addr::{InternetAddr, InternetAddrStr},
    proxy_table::{
        ProxyAction, ProxyConfig, ProxyConfigBuilder, ProxyGroup, ProxyGroupBuilder, ProxyTable,
        ProxyTableBuilder, ProxyTableEntry, WeightedProxyChain,
    },
};

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
