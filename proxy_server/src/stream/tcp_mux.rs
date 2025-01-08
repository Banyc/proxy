use std::sync::Arc;

use common::loading;
use protocol::stream::{context::ConcreteStreamContext, streams::tcp_mux::TcpMuxServer};
use serde::Deserialize;
use thiserror::Error;
use tokio::net::{TcpListener, ToSocketAddrs};
use tracing::error;

use crate::ListenerBindError;

use super::{
    StreamProxyConnHandler, StreamProxyConnHandlerBuilder, StreamProxyServerBuildError,
    StreamProxyServerConfig,
};

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TcpMuxProxyServerConfig {
    pub listen_addr: Arc<str>,
    #[serde(flatten)]
    pub inner: StreamProxyServerConfig,
}
impl TcpMuxProxyServerConfig {
    pub fn into_builder(self, stream_context: ConcreteStreamContext) -> TcpMuxProxyServerBuilder {
        let listen_addr = Arc::clone(&self.listen_addr);
        let inner = self.inner.into_builder(stream_context, listen_addr);
        TcpMuxProxyServerBuilder {
            listen_addr: self.listen_addr,
            inner,
        }
    }
}

#[derive(Debug, Clone)]
pub struct TcpMuxProxyServerBuilder {
    pub listen_addr: Arc<str>,
    pub inner: StreamProxyConnHandlerBuilder,
}
impl loading::Build for TcpMuxProxyServerBuilder {
    type ConnHandler = StreamProxyConnHandler;
    type Server = TcpMuxServer<Self::ConnHandler>;
    type Err = TcpMuxProxyServerBuildError;

    async fn build_server(self) -> Result<Self::Server, Self::Err> {
        let listen_addr = self.listen_addr.clone();
        let stream_proxy = self.build_conn_handler()?;
        build_tcp_mux_proxy_server(listen_addr.as_ref(), stream_proxy)
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
pub enum TcpMuxProxyServerBuildError {
    #[error("{0}")]
    Hook(#[from] StreamProxyServerBuildError),
    #[error("{0}")]
    Server(#[from] ListenerBindError),
}
pub async fn build_tcp_mux_proxy_server(
    listen_addr: impl ToSocketAddrs,
    stream_proxy: StreamProxyConnHandler,
) -> Result<TcpMuxServer<StreamProxyConnHandler>, ListenerBindError> {
    let listener = TcpListener::bind(listen_addr)
        .await
        .map_err(ListenerBindError)?;
    let server = TcpMuxServer::new(listener, stream_proxy);
    Ok(server)
}
