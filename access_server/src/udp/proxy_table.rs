use common::{
    addr::InternetAddr,
    proxy_table::{ProxyGroupBuildContext, ProxyTableBuildContext},
};
use proxy_client::udp::UdpTracerBuilder;

pub type UdpProxyTableBuildContext<'caller> =
    ProxyTableBuildContext<'caller, InternetAddr, UdpTracerBuilder>;
pub type UdpProxyGroupBuildContext<'caller> =
    ProxyGroupBuildContext<'caller, InternetAddr, UdpTracerBuilder>;
