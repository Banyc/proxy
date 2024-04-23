use std::sync::Arc;

use common::loading;
use protocol::stream::{context::ConcreteStreamContext, streams::rtp::RtpServer};
use serde::Deserialize;
use thiserror::Error;
use tokio::net::ToSocketAddrs;

use crate::ListenerBindError;

use super::{
    StreamProxyServer, StreamProxyServerBuildError, StreamProxyServerBuilder,
    StreamProxyServerConfig,
};

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RtpProxyServerConfig {
    pub listen_addr: Arc<str>,
    #[serde(flatten)]
    pub inner: StreamProxyServerConfig,
}

impl RtpProxyServerConfig {
    pub fn into_builder(self, stream_context: ConcreteStreamContext) -> RtpProxyServerBuilder {
        let inner = self.inner.into_builder(stream_context);
        RtpProxyServerBuilder {
            listen_addr: self.listen_addr,
            inner,
        }
    }
}

#[derive(Debug, Clone)]
pub struct RtpProxyServerBuilder {
    pub listen_addr: Arc<str>,
    pub inner: StreamProxyServerBuilder,
}
impl loading::Builder for RtpProxyServerBuilder {
    type Hook = StreamProxyServer;
    type Server = RtpServer<Self::Hook>;
    type Err = RtpProxyServerBuildError;

    async fn build_server(self) -> Result<Self::Server, Self::Err> {
        let listen_addr = self.listen_addr.clone();
        let stream_proxy = self.build_hook()?;
        build_rtp_proxy_server(listen_addr.as_ref(), stream_proxy)
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

#[derive(Debug, Error)]
pub enum RtpProxyServerBuildError {
    #[error("{0}")]
    Hook(#[from] StreamProxyServerBuildError),
    #[error("{0}")]
    Server(#[from] ListenerBindError),
}

pub async fn build_rtp_proxy_server(
    listen_addr: impl ToSocketAddrs,
    stream_proxy: StreamProxyServer,
) -> Result<RtpServer<StreamProxyServer>, ListenerBindError> {
    let listener = rtp::udp::Listener::bind(listen_addr)
        .await
        .map_err(ListenerBindError)?;
    let server = RtpServer::new(listener, stream_proxy);
    Ok(server)
}
