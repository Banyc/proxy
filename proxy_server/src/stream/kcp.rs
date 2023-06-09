use std::{io, sync::Arc};

use async_trait::async_trait;
use common::{
    loading,
    stream::streams::kcp::{fast_kcp_config, KcpServer},
};
use serde::Deserialize;
use thiserror::Error;
use tokio::net::ToSocketAddrs;
use tokio_kcp::KcpListener;
use tracing::error;

use super::{StreamProxy, StreamProxyBuilder};

#[derive(Debug, Clone, PartialEq, Eq, Hash, Deserialize)]
pub struct KcpProxyServerBuilder {
    pub listen_addr: Arc<str>,
    #[serde(flatten)]
    pub inner: StreamProxyBuilder,
}

#[async_trait]
impl loading::Builder for KcpProxyServerBuilder {
    type Hook = StreamProxy;
    type Server = KcpServer<Self::Hook>;

    async fn build_server(self) -> io::Result<Self::Server> {
        let stream_proxy = self.inner.build();
        build_kcp_proxy_server(self.listen_addr.as_ref(), stream_proxy)
            .await
            .map_err(|e| e.0)
    }

    fn build_hook(self) -> io::Result<Self::Hook> {
        Ok(self.inner.build())
    }

    fn key(&self) -> &Arc<str> {
        &self.listen_addr
    }
}

impl KcpProxyServerBuilder {
    pub async fn build(self) -> Result<KcpServer<StreamProxy>, ListenerBindError> {
        let stream_proxy = self.inner.build();
        build_kcp_proxy_server(self.listen_addr.as_ref(), stream_proxy).await
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

#[derive(Debug, Error)]
#[error("Failed to bind to listen address")]
pub struct ListenerBindError(#[source] io::Error);
