use std::sync::Arc;

use common::{
    loading,
    stream::concrete::{
        context::StreamContext,
        streams::kcp::{fast_kcp_config, KcpServer},
    },
};
use serde::Deserialize;
use tokio::net::ToSocketAddrs;
use tokio_kcp::KcpListener;

use crate::ListenerBindError;

use super::{StreamProxy, StreamProxyBuilder, StreamProxyConfig, StreamProxyServerBuildError};

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct KcpProxyServerConfig {
    pub listen_addr: Arc<str>,
    #[serde(flatten)]
    pub inner: StreamProxyConfig,
}

impl KcpProxyServerConfig {
    pub fn into_builder(self, stream_context: StreamContext) -> KcpProxyServerBuilder {
        let inner = self.inner.into_builder(stream_context);
        KcpProxyServerBuilder {
            listen_addr: self.listen_addr,
            inner,
        }
    }
}

#[derive(Debug, Clone)]
pub struct KcpProxyServerBuilder {
    pub listen_addr: Arc<str>,
    pub inner: StreamProxyBuilder,
}

impl loading::Builder for KcpProxyServerBuilder {
    type Hook = StreamProxy;
    type Server = KcpServer<Self::Hook>;
    type Err = StreamProxyServerBuildError;

    async fn build_server(self) -> Result<Self::Server, Self::Err> {
        let listen_addr = self.listen_addr.clone();
        let stream_proxy = self.build_hook()?;
        build_kcp_proxy_server(listen_addr.as_ref(), stream_proxy)
            .await
            .map_err(|e| e.into())
    }

    fn build_hook(self) -> Result<Self::Hook, Self::Err> {
        self.inner.build().map_err(|e| e.into())
    }

    fn key(&self) -> &Arc<str> {
        &self.listen_addr
    }
}

pub async fn build_kcp_proxy_server(
    listen_addr: impl ToSocketAddrs,
    stream_proxy: StreamProxy,
) -> Result<KcpServer<StreamProxy>, ListenerBindError> {
    let config = fast_kcp_config();
    let listener = KcpListener::bind(config, listen_addr)
        .await
        .map_err(|e| ListenerBindError(e.into()))?;
    let server = KcpServer::new(listener, stream_proxy);
    Ok(server)
}
