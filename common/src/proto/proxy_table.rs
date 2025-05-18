use crate::{
    addr::{InternetAddr, InternetAddrStr},
    proxy_table::{
        ProxyAction, ProxyConfig, ProxyConfigBuilder, ProxyGroup, ProxyGroupBuildContext,
        ProxyGroupBuilder, ProxyTable, ProxyTableBuildContext, ProxyTableBuilder, ProxyTableEntry,
        WeightedProxyChain,
    },
};

use super::{
    addr::{StreamAddr, StreamAddrStr},
    client::{stream::StreamTracerBuilder, udp::UdpTracerBuilder},
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
pub type StreamProxyTableBuildContext<'caller> =
    ProxyTableBuildContext<'caller, StreamAddr, StreamTracerBuilder>;
pub type StreamProxyGroupBuildContext<'caller> =
    ProxyGroupBuildContext<'caller, StreamAddr, StreamTracerBuilder>;

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
pub type UdpProxyTableBuildContext<'caller> =
    ProxyTableBuildContext<'caller, InternetAddr, UdpTracerBuilder>;
pub type UdpProxyGroupBuildContext<'caller> =
    ProxyGroupBuildContext<'caller, InternetAddr, UdpTracerBuilder>;
