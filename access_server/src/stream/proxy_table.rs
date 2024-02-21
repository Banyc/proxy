use common::proxy_table::{ProxyGroupBuildContext, ProxyTableBuildContext};
use protocol::stream::addr::ConcreteStreamAddr;
use proxy_client::stream::StreamTracerBuilder;

pub type StreamProxyTableBuildContext<'caller> =
    ProxyTableBuildContext<'caller, ConcreteStreamAddr, StreamTracerBuilder>;
pub type StreamProxyGroupBuildContext<'caller> =
    ProxyGroupBuildContext<'caller, ConcreteStreamAddr, StreamTracerBuilder>;
