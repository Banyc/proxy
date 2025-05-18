use common::{
    proto::addr::StreamAddr,
    proxy_table::{ProxyGroupBuildContext, ProxyTableBuildContext},
};
use proxy_client::stream::StreamTracerBuilder;

pub type StreamProxyTableBuildContext<'caller> =
    ProxyTableBuildContext<'caller, StreamAddr, StreamTracerBuilder>;
pub type StreamProxyGroupBuildContext<'caller> =
    ProxyGroupBuildContext<'caller, StreamAddr, StreamTracerBuilder>;
