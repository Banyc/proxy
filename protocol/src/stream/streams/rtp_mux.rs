use common::{
    loading,
    proto::{
        conn_handler::{
            ListenerBindError,
            stream::{
                StreamProxyConnHandlerBuilder, StreamProxyConnHandlerConfig,
                StreamProxyServerBuildError,
            },
        },
        context::{Runtime, UdpRuntime},
    },
    session::SessionSpawner,
};
use serde::Deserialize;
use std::sync::Arc;
use thiserror::Error;
use tokio::net::ToSocketAddrs;
mod connector;
mod server;
pub use super::mux::{ConnectorDriverError, MuxConnectorDriver};
use crate::stream::streams::mux::{
    MuxProxyHandler, MuxProxyUdpBuildError, build_udp_proxy_handler,
};
pub use connector::RtpMuxConnector;
pub use server::{RtpMuxServer, ServeError};
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RtpMuxProxyServerConfig {
    pub listen_addr: Arc<str>,
    #[serde(default)]
    pub fec: bool,
    #[serde(flatten)]
    pub inner: StreamProxyConnHandlerConfig,
}
impl RtpMuxProxyServerConfig {
    pub fn into_builder(self, runtime: Runtime) -> RtpMuxProxyServerBuilder {
        let listen_addr = Arc::clone(&self.listen_addr);
        let inner = self.inner.into_builder(runtime.stream, listen_addr);
        RtpMuxProxyServerBuilder {
            listen_addr: self.listen_addr,
            fec: self.fec,
            inner,
            udp_context: runtime.udp,
        }
    }
}
#[derive(Debug, Clone)]
pub struct RtpMuxProxyServerBuilder {
    pub listen_addr: Arc<str>,
    pub fec: bool,
    pub inner: StreamProxyConnHandlerBuilder,
    pub udp_context: UdpRuntime,
}
impl loading::Build for RtpMuxProxyServerBuilder {
    type ConnHandler = MuxProxyHandler;
    type Server = RtpMuxServer<Self::ConnHandler>;
    type Err = RtpMuxProxyServerBuildError;
    async fn build_server(self) -> Result<Self::Server, Self::Err> {
        let listen_addr = self.listen_addr.clone();
        let fec = self.fec;
        let session_spawner = self.inner.stream_context.session_spawner.clone();
        let handler = self.build_conn_handler()?;
        build_rtp_mux_proxy_server(listen_addr.as_ref(), handler, fec, session_spawner)
            .await
            .map_err(Into::into)
    }
    fn build_conn_handler(self) -> Result<Self::ConnHandler, Self::Err> {
        let stream = self
            .inner
            .clone()
            .build()
            .map_err(RtpMuxProxyServerBuildError::Hook)?;
        let udp = build_udp_proxy_handler(
            self.inner.header_key,
            self.inner.payload_key,
            self.udp_context,
            self.inner.allow_loopback,
        )
        .map_err(RtpMuxProxyServerBuildError::Udp)?;
        Ok(MuxProxyHandler { stream, udp })
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
    Udp(#[from] MuxProxyUdpBuildError),
    #[error("{0}")]
    Server(#[from] ListenerBindError),
}
pub async fn build_rtp_mux_proxy_server(
    listen_addr: impl ToSocketAddrs + Clone + std::fmt::Debug,
    handler: MuxProxyHandler,
    fec: bool,
    session_spawner: SessionSpawner,
) -> Result<RtpMuxServer<MuxProxyHandler>, ListenerBindError> {
    let server = ::rtp_mux::RtpMuxServer::bind(listen_addr, fec)
        .await
        .map_err(ListenerBindError)?;
    Ok(RtpMuxServer::from_core(server, handler, session_spawner))
}
