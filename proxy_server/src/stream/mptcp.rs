use std::{num::NonZeroUsize, sync::Arc};

use common::{
    loading,
    stream::{
        concrete::{addr::ConcreteStreamType, pool::Pool, streams::mptcp::MptcpServer},
        session_table::StreamSessionTable,
    },
};
use mptcp::MptcpListener;
use serde::Deserialize;
use thiserror::Error;
use tokio::net::ToSocketAddrs;
use tracing::error;

use crate::ListenerBindError;

use super::{StreamProxy, StreamProxyBuildError, StreamProxyBuilder, StreamProxyConfig};

const MAX_SESSION_STREAMS: usize = 4;

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MptcpProxyServerConfig {
    pub listen_addr: Arc<str>,
    #[serde(flatten)]
    pub inner: StreamProxyConfig,
}

impl MptcpProxyServerConfig {
    pub fn into_builder(
        self,
        stream_pool: Pool,
        session_table: Option<StreamSessionTable<ConcreteStreamType>>,
    ) -> MptcpProxyServerBuilder {
        let inner = self.inner.into_builder(stream_pool, session_table);
        MptcpProxyServerBuilder {
            listen_addr: self.listen_addr,
            inner,
        }
    }
}

#[derive(Debug, Clone)]
pub struct MptcpProxyServerBuilder {
    pub listen_addr: Arc<str>,
    pub inner: StreamProxyBuilder,
}

impl loading::Builder for MptcpProxyServerBuilder {
    type Hook = StreamProxy;
    type Server = MptcpServer<Self::Hook>;
    type Err = MptcpProxyServerBuildError;

    async fn build_server(self) -> Result<Self::Server, Self::Err> {
        let listen_addr = self.listen_addr.clone();
        let stream_proxy = self.build_hook()?;
        build_mptcp_proxy_server(listen_addr.as_ref(), stream_proxy)
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
pub enum MptcpProxyServerBuildError {
    #[error("{0}")]
    Hook(#[from] StreamProxyBuildError),
    #[error("{0}")]
    Server(#[from] ListenerBindError),
}

pub async fn build_mptcp_proxy_server(
    listen_addr: impl ToSocketAddrs,
    stream_proxy: StreamProxy,
) -> Result<MptcpServer<StreamProxy>, ListenerBindError> {
    let listener =
        MptcpListener::bind(listen_addr, NonZeroUsize::new(MAX_SESSION_STREAMS).unwrap())
            .await
            .map_err(ListenerBindError)?;
    let server = MptcpServer::new(listener, stream_proxy);
    Ok(server)
}
