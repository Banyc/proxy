use std::{collections::HashMap, io, sync::Arc};

use async_speed_limit::Limiter;
use common::{
    config::SharableConfig,
    loading,
    proxy_table::ProxyGroupBuildError,
    stream::{
        io_copy::{CopyBidirectional, LogContext},
        IoAddr, IoStream, StreamServerHook,
    },
};
use protocol::stream::{
    addr::{ConcreteStreamAddr, ConcreteStreamAddrStr},
    context::ConcreteStreamContext,
    proxy_table::{StreamProxyGroup, StreamProxyGroupBuilder},
    streams::tcp::TcpServer,
};
use proxy_client::stream::{establish, StreamEstablishError};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::net::ToSocketAddrs;
use tracing::{error, instrument, warn};

use crate::stream::proxy_table::StreamProxyGroupBuildContext;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TcpAccessServerConfig {
    pub listen_addr: Arc<str>,
    pub destination: ConcreteStreamAddrStr,
    pub proxy_group: SharableConfig<StreamProxyGroupBuilder>,
    pub speed_limit: Option<f64>,
}

impl TcpAccessServerConfig {
    pub fn into_builder(
        self,
        proxy_group: &HashMap<Arc<str>, StreamProxyGroup>,
        proxy_group_cx: StreamProxyGroupBuildContext<'_>,
        stream_context: ConcreteStreamContext,
    ) -> Result<TcpAccessServerBuilder, BuildError> {
        let proxy_group = match self.proxy_group {
            SharableConfig::SharingKey(key) => proxy_group
                .get(&key)
                .ok_or_else(|| BuildError::ProxyGroupKeyNotFound(key.clone()))?
                .clone(),
            SharableConfig::Private(x) => x.build(proxy_group_cx.clone())?,
        };

        Ok(TcpAccessServerBuilder {
            listen_addr: self.listen_addr,
            destination: self.destination,
            proxy_group,
            speed_limit: self.speed_limit.unwrap_or(f64::INFINITY),
            stream_context,
        })
    }
}

#[derive(Debug, Error)]
pub enum BuildError {
    #[error("Proxy group key not found: {0}")]
    ProxyGroupKeyNotFound(Arc<str>),
    #[error("{0}")]
    ProxyGroup(#[from] ProxyGroupBuildError),
}

#[derive(Debug, Clone)]
pub struct TcpAccessServerBuilder {
    listen_addr: Arc<str>,
    destination: ConcreteStreamAddrStr,
    proxy_group: StreamProxyGroup,
    speed_limit: f64,
    stream_context: ConcreteStreamContext,
}

impl loading::Builder for TcpAccessServerBuilder {
    type Hook = TcpAccess;
    type Server = TcpServer<Self::Hook>;
    type Err = io::Error;

    async fn build_server(self) -> Result<Self::Server, Self::Err> {
        let listen_addr = self.listen_addr.clone();
        let access = self.build_hook()?;
        let server = access.build(listen_addr.as_ref()).await?;
        Ok(server)
    }

    fn key(&self) -> &Arc<str> {
        &self.listen_addr
    }

    fn build_hook(self) -> Result<Self::Hook, Self::Err> {
        Ok(TcpAccess::new(
            self.proxy_group,
            self.destination.0,
            self.speed_limit,
            self.stream_context,
        ))
    }
}

#[derive(Debug)]
pub struct TcpAccess {
    proxy_group: StreamProxyGroup,
    destination: ConcreteStreamAddr,
    speed_limiter: Limiter,
    stream_context: ConcreteStreamContext,
}

impl TcpAccess {
    pub fn new(
        proxy_group: StreamProxyGroup,
        destination: ConcreteStreamAddr,
        speed_limit: f64,
        stream_context: ConcreteStreamContext,
    ) -> Self {
        Self {
            proxy_group,
            destination,
            speed_limiter: Limiter::new(speed_limit),
            stream_context,
        }
    }

    pub async fn build(self, listen_addr: impl ToSocketAddrs) -> io::Result<TcpServer<Self>> {
        let tcp_listener = tokio::net::TcpListener::bind(listen_addr).await?;
        Ok(TcpServer::new(tcp_listener, self))
    }

    async fn proxy<S>(&self, downstream: S) -> Result<(), ProxyError>
    where
        S: IoStream + IoAddr,
    {
        let proxy_chain = self.proxy_group.choose_chain();
        let upstream = establish(
            &proxy_chain.chain,
            self.destination.clone(),
            &self.stream_context,
        )
        .await?;

        let metrics_context = LogContext {
            start: (std::time::Instant::now(), std::time::SystemTime::now()),
            upstream_addr: upstream.addr,
            upstream_sock_addr: upstream.sock_addr,
            downstream_addr: downstream.peer_addr().ok(),
            upstream_local: upstream.stream.local_addr().ok(),
            session_table: self.stream_context.session_table.clone(),
            destination: Some(self.destination.clone()),
        };
        let io_copy = CopyBidirectional {
            downstream,
            upstream: upstream.stream,
            payload_crypto: proxy_chain.payload_crypto.clone(),
            speed_limiter: self.speed_limiter.clone(),
            metrics_context,
        }
        .serve_as_access_server("TCP");
        tokio::spawn(async move {
            let _ = io_copy.await;
        });
        Ok(())
    }
}

#[derive(Debug, Error)]
pub enum ProxyError {
    #[error("Failed to get downstream address: {0}")]
    DownstreamAddr(#[source] io::Error),
    #[error("Failed to establish proxy chain: {0}")]
    EstablishProxyChain(#[from] StreamEstablishError),
}

impl loading::Hook for TcpAccess {}

impl StreamServerHook for TcpAccess {
    #[instrument(skip(self, stream))]
    async fn handle_stream<S>(&self, stream: S)
    where
        S: IoStream + IoAddr,
    {
        match self.proxy(stream).await {
            Ok(()) => (),
            Err(e) => warn!(?e, "Failed to proxy"),
        }
    }
}
