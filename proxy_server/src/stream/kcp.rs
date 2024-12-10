use std::sync::Arc;

use common::loading;
use protocol::stream::{
    context::ConcreteStreamContext,
    streams::kcp::{fast_kcp_config, KcpServer},
};
use serde::Deserialize;
use thiserror::Error;
use tokio::net::ToSocketAddrs;
use tokio_kcp::KcpListener;

use crate::ListenerBindError;

use super::{
    StreamProxyServer, StreamProxyServerBuildError, StreamProxyServerBuilder,
    StreamProxyServerConfig,
};

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct KcpProxyServerConfig {
    pub listen_addr: Arc<str>,
    #[serde(flatten)]
    pub inner: StreamProxyServerConfig,
}
impl KcpProxyServerConfig {
    pub fn into_builder(self, stream_context: ConcreteStreamContext) -> KcpProxyServerBuilder {
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
    pub inner: StreamProxyServerBuilder,
}
impl loading::Build for KcpProxyServerBuilder {
    type ConnHandler = StreamProxyServer;
    type Server = KcpServer<Self::ConnHandler>;
    type Err = KcpProxyServerBuildError;

    async fn build_server(self) -> Result<Self::Server, Self::Err> {
        let listen_addr = self.listen_addr.clone();
        let stream_proxy = self.build_conn_handler()?;
        build_kcp_proxy_server(listen_addr.as_ref(), stream_proxy)
            .await
            .map_err(|e| e.into())
    }

    fn build_conn_handler(self) -> Result<Self::ConnHandler, Self::Err> {
        self.inner.build().map_err(|e| e.into())
    }

    fn key(&self) -> &Arc<str> {
        &self.listen_addr
    }
}
#[derive(Debug, Error)]
pub enum KcpProxyServerBuildError {
    #[error("{0}")]
    Hook(#[from] StreamProxyServerBuildError),
    #[error("{0}")]
    Server(#[from] ListenerBindError),
}
pub async fn build_kcp_proxy_server(
    listen_addr: impl ToSocketAddrs,
    stream_proxy: StreamProxyServer,
) -> Result<KcpServer<StreamProxyServer>, ListenerBindError> {
    let config = fast_kcp_config();
    let listener = KcpListener::bind(config, listen_addr)
        .await
        .map_err(|e| ListenerBindError(e.into()))?;
    let server = KcpServer::new(listener, stream_proxy);
    Ok(server)
}
