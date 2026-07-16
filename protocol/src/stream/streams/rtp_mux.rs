use common::{
    loading,
    proto::{
        conn_handler::{
            ListenerBindError,
            stream::{
                StreamProxyConnHandler, StreamProxyConnHandlerBuilder,
                StreamProxyConnHandlerConfig, StreamProxyServerBuildError,
            },
        },
        context::StreamContext,
    },
};
use serde::Deserialize;
use std::sync::Arc;
use thiserror::Error;
use tokio::net::ToSocketAddrs;
mod connector;
mod server;
pub use connector::RtpMuxConnector;
pub use server::{RtpMuxServer, ServeError};
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RtpMuxListenerConfig {
    pub listen_addr: Arc<str>,
    pub fec: bool,
}
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RtpMuxProxyServerConfig {
    #[serde(flatten)]
    pub listener: RtpMuxListenerConfig,
    #[serde(flatten)]
    pub inner: StreamProxyConnHandlerConfig,
}
impl RtpMuxProxyServerConfig {
    pub fn into_builder(self, stream_context: StreamContext) -> RtpMuxProxyServerBuilder {
        let listen_addr = Arc::clone(&self.listener.listen_addr);
        let inner = self.inner.into_builder(stream_context, listen_addr);
        RtpMuxProxyServerBuilder {
            listener: self.listener,
            inner,
        }
    }
}
#[derive(Debug, Clone)]
pub struct RtpMuxProxyServerBuilder {
    pub listener: RtpMuxListenerConfig,
    pub inner: StreamProxyConnHandlerBuilder,
}
impl loading::Build for RtpMuxProxyServerBuilder {
    type ConnHandler = StreamProxyConnHandler;
    type Server = RtpMuxServer<Self::ConnHandler>;
    type Err = RtpMuxProxyServerBuildError;
    async fn build_server(self) -> Result<Self::Server, Self::Err> {
        let listener = self.listener.clone();
        let stream_proxy = self.build_conn_handler()?;
        build_rtp_mux_proxy_server(listener.listen_addr.as_ref(), stream_proxy, listener.fec)
            .await
            .map_err(Into::into)
    }
    fn build_conn_handler(self) -> Result<Self::ConnHandler, Self::Err> {
        self.inner.build().map_err(Into::into)
    }
    fn key(&self) -> &Arc<str> {
        &self.listener.listen_addr
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
    listen_addr: impl ToSocketAddrs + Clone + std::fmt::Debug,
    stream_proxy: StreamProxyConnHandler,
    fec: bool,
) -> Result<RtpMuxServer<StreamProxyConnHandler>, ListenerBindError> {
    let server = ::rtp_mux::RtpMuxServer::bind(listen_addr, fec)
        .await
        .map_err(ListenerBindError)?;
    Ok(RtpMuxServer::from_core(server, stream_proxy))
}
