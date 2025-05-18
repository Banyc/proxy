use common::{
    proxy_table::{ProxyGroupBuildContext, ProxyTableBuildContext},
    stream::addr::StreamAddr,
};
use proxy_client::stream::StreamTracerBuilder;

pub type StreamProxyTableBuildContext<'caller> =
    ProxyTableBuildContext<'caller, StreamAddr, StreamTracerBuilder>;
pub type StreamProxyGroupBuildContext<'caller> =
    ProxyGroupBuildContext<'caller, StreamAddr, StreamTracerBuilder>;
