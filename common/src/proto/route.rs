use crate::{
    addr::{InternetAddr, InternetAddrStr},
    route::{
        ConnConfig, ConnConfigBuilder, ConnSelector, ConnSelectorBuildContext, ConnSelectorBuilder,
        RouteAction, RouteTable, RouteTableBuildContext, RouteTableBuilder, RouteTableEntry,
        WeightedConnChain,
    },
};

use super::{
    addr::{StreamAddr, StreamAddrStr},
    client::{stream::StreamTracerBuilder, udp::UdpTracerBuilder},
};

pub type StreamConnConfigBuilder = ConnConfigBuilder<StreamAddrStr>;
pub type StreamConnConfig = ConnConfig<StreamAddr>;
pub type StreamProxyChain = [StreamConnConfig];
pub type StreamWeightedConnChain = WeightedConnChain<StreamAddr>;
pub type StreamRouteTable = RouteTable<StreamAddr>;
pub type StreamRouteTableEntry = RouteTableEntry<StreamAddr>;
pub type StreamRouteTableEntryAction = RouteAction<StreamAddr>;
pub type StreamRouteGroup = ConnSelector<StreamAddr>;
pub type StreamRouteTableBuilder = RouteTableBuilder<StreamAddrStr>;
pub type StreamConnSelectorBuilder = ConnSelectorBuilder<StreamAddrStr>;
pub type StreamRouteTableBuildContext<'caller> =
    RouteTableBuildContext<'caller, StreamAddr, StreamTracerBuilder>;
pub type StreamConnSelectorBuildContext<'caller> =
    ConnSelectorBuildContext<'caller, StreamAddr, StreamTracerBuilder>;

pub type UdpConnConfigBuilder = ConnConfigBuilder<InternetAddrStr>;
pub type UdpConnConfig = ConnConfig<InternetAddr>;
pub type UdpConnChain = [UdpConnConfig];
pub type UdpWeightedConnChain = WeightedConnChain<InternetAddr>;
pub type UdpRouteTable = RouteTable<InternetAddr>;
pub type UdpRouteTableEntry = RouteTableEntry<InternetAddr>;
pub type UdpRouteTableEntryAction = RouteAction<InternetAddr>;
pub type UdpConnSelector = ConnSelector<InternetAddr>;
pub type UdpRouteTableBuilder = RouteTableBuilder<InternetAddrStr>;
pub type UdpConnSelectorBuilder = ConnSelectorBuilder<InternetAddrStr>;
pub type UdpRouteTableBuildContext<'caller> =
    RouteTableBuildContext<'caller, InternetAddr, UdpTracerBuilder>;
pub type UdpConnSelectorBuildContext<'caller> =
    ConnSelectorBuildContext<'caller, InternetAddr, UdpTracerBuilder>;
