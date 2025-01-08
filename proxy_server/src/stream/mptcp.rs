use std::{num::NonZeroUsize, sync::Arc};

use common::loading;
use mptcp::MptcpListener;
use protocol::stream::{context::ConcreteStreamContext, streams::mptcp::MptcpServer};
use serde::Deserialize;
use thiserror::Error;
use tokio::net::ToSocketAddrs;
use tracing::error;

use crate::ListenerBindError;

use super::{
    StreamProxyConnHandler, StreamProxyConnHandlerBuilder, StreamProxyServerBuildError,
    StreamProxyServerConfig,
};

const MAX_SESSION_STREAMS: usize = 4;

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MptcpProxyServerConfig {
    pub listen_addr: Arc<str>,
    #[serde(flatten)]
    pub inner: StreamProxyServerConfig,
}
impl MptcpProxyServerConfig {
    pub fn into_builder(self, stream_context: ConcreteStreamContext) -> MptcpProxyServerBuilder {
        let listen_addr = Arc::clone(&self.listen_addr);
        let inner = self.inner.into_builder(stream_context, listen_addr);
        MptcpProxyServerBuilder {
            listen_addr: self.listen_addr,
            inner,
        }
    }
}

#[derive(Debug, Clone)]
pub struct MptcpProxyServerBuilder {
    pub listen_addr: Arc<str>,
    pub inner: StreamProxyConnHandlerBuilder,
}
impl loading::Build for MptcpProxyServerBuilder {
    type ConnHandler = StreamProxyConnHandler;
    type Server = MptcpServer<Self::ConnHandler>;
    type Err = MptcpProxyServerBuildError;

    async fn build_server(self) -> Result<Self::Server, Self::Err> {
        let listen_addr = self.listen_addr.clone();
        let stream_proxy = self.build_conn_handler()?;
        build_mptcp_proxy_server(listen_addr.as_ref(), stream_proxy)
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
pub enum MptcpProxyServerBuildError {
    #[error("{0}")]
    Hook(#[from] StreamProxyServerBuildError),
    #[error("{0}")]
    Server(#[from] ListenerBindError),
}
pub async fn build_mptcp_proxy_server(
    listen_addr: impl ToSocketAddrs,
    stream_proxy: StreamProxyConnHandler,
) -> Result<MptcpServer<StreamProxyConnHandler>, ListenerBindError> {
    let listener = MptcpListener::bind(
        [listen_addr].iter(),
        NonZeroUsize::new(MAX_SESSION_STREAMS).unwrap(),
    )
    .await
    .map_err(ListenerBindError)?;
    let server = MptcpServer::new(listener, stream_proxy);
    Ok(server)
}
