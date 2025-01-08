use std::sync::Arc;

use common::loading;
use protocol::stream::{context::ConcreteStreamContext, streams::rtp_mux::RtpMuxServer};
use serde::Deserialize;
use thiserror::Error;
use tokio::net::ToSocketAddrs;
use tracing::error;

use crate::ListenerBindError;

use super::{
    StreamProxyConnHandler, StreamProxyConnHandlerBuilder, StreamProxyServerBuildError,
    StreamProxyServerConfig,
};

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RtpMuxProxyServerConfig {
    pub listen_addr: Arc<str>,
    #[serde(flatten)]
    pub inner: StreamProxyServerConfig,
}
impl RtpMuxProxyServerConfig {
    pub fn into_builder(self, stream_context: ConcreteStreamContext) -> RtpMuxProxyServerBuilder {
        let listen_addr = Arc::clone(&self.listen_addr);
        let inner = self.inner.into_builder(stream_context, listen_addr);
        RtpMuxProxyServerBuilder {
            listen_addr: self.listen_addr,
            inner,
        }
    }
}

#[derive(Debug, Clone)]
pub struct RtpMuxProxyServerBuilder {
    pub listen_addr: Arc<str>,
    pub inner: StreamProxyConnHandlerBuilder,
}
impl loading::Build for RtpMuxProxyServerBuilder {
    type ConnHandler = StreamProxyConnHandler;
    type Server = RtpMuxServer<Self::ConnHandler>;
    type Err = RtpMuxProxyServerBuildError;

    async fn build_server(self) -> Result<Self::Server, Self::Err> {
        let listen_addr = self.listen_addr.clone();
        let stream_proxy = self.build_conn_handler()?;
        build_rtp_mux_proxy_server(listen_addr.as_ref(), stream_proxy)
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
pub enum RtpMuxProxyServerBuildError {
    #[error("{0}")]
    Hook(#[from] StreamProxyServerBuildError),
    #[error("{0}")]
    Server(#[from] ListenerBindError),
}
pub async fn build_rtp_mux_proxy_server(
    listen_addr: impl ToSocketAddrs,
    stream_proxy: StreamProxyConnHandler,
) -> Result<RtpMuxServer<StreamProxyConnHandler>, ListenerBindError> {
    let listener = rtp::udp::Listener::bind(listen_addr)
        .await
        .map_err(ListenerBindError)?;
    let server = RtpMuxServer::new(listener, stream_proxy);
    Ok(server)
}
