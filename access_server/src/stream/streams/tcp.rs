use std::{collections::HashMap, io, sync::Arc};

use async_speed_limit::Limiter;
use common::{
    config::SharableConfig,
    loading,
    proto::{
        addr::{StreamAddr, StreamAddrStr},
        context::StreamContext,
        io_copy::stream::{ConnContext, CopyBidirectional},
        proxy_table::{StreamProxyGroup, StreamProxyGroupBuilder},
    },
    proxy_table::ProxyGroupBuildError,
    stream::{HasIoAddr, OwnIoStream, StreamServerHandleConn},
};
use protocol::stream::streams::tcp::TcpServer;
use proxy_client::stream::{StreamEstablishError, establish};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tracing::{error, instrument, warn};

use crate::stream::proxy_table::StreamProxyGroupBuildContext;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TcpAccessServerConfig {
    pub listen_addr: Arc<str>,
    pub destination: StreamAddrStr,
    pub proxy_group: SharableConfig<StreamProxyGroupBuilder>,
    pub speed_limit: Option<f64>,
}
impl TcpAccessServerConfig {
    pub fn into_builder(
        self,
        proxy_group: &HashMap<Arc<str>, StreamProxyGroup>,
        proxy_group_cx: StreamProxyGroupBuildContext<'_>,
        stream_context: StreamContext,
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
    destination: StreamAddrStr,
    proxy_group: StreamProxyGroup,
    speed_limit: f64,
    stream_context: StreamContext,
}
impl loading::Build for TcpAccessServerBuilder {
    type ConnHandler = TcpAccessConnHandler;
    type Server = TcpServer<Self::ConnHandler>;
    type Err = io::Error;

    async fn build_server(self) -> Result<Self::Server, Self::Err> {
        let listen_addr = self.listen_addr.clone();
        let access = self.build_conn_handler()?;
        let tcp_listener = tokio::net::TcpListener::bind(listen_addr.as_ref()).await?;
        let server = TcpServer::new(tcp_listener, access);
        Ok(server)
    }

    fn key(&self) -> &Arc<str> {
        &self.listen_addr
    }

    fn build_conn_handler(self) -> Result<Self::ConnHandler, Self::Err> {
        Ok(TcpAccessConnHandler::new(
            self.proxy_group,
            self.destination.0,
            self.speed_limit,
            self.stream_context,
            Arc::clone(&self.listen_addr),
        ))
    }
}

#[derive(Debug)]
pub struct TcpAccessConnHandler {
    proxy_group: StreamProxyGroup,
    destination: StreamAddr,
    speed_limiter: Limiter,
    stream_context: StreamContext,
    listen_addr: Arc<str>,
}
impl TcpAccessConnHandler {
    pub fn new(
        proxy_group: StreamProxyGroup,
        destination: StreamAddr,
        speed_limit: f64,
        stream_context: StreamContext,
        listen_addr: Arc<str>,
    ) -> Self {
        Self {
            proxy_group,
            destination,
            speed_limiter: Limiter::new(speed_limit),
            stream_context,
            listen_addr,
        }
    }

    async fn proxy<Downstream>(&self, downstream: Downstream) -> Result<(), ProxyError>
    where
        Downstream: OwnIoStream + HasIoAddr,
    {
        let proxy_chain = self.proxy_group.choose_chain();
        let upstream = establish(
            &proxy_chain.chain,
            self.destination.clone(),
            &self.stream_context,
        )
        .await?;

        let conn_context = ConnContext {
            start: (std::time::Instant::now(), std::time::SystemTime::now()),
            upstream_remote: upstream.addr,
            upstream_remote_sock: upstream.sock_addr,
            upstream_local: upstream.stream.local_addr().ok(),
            downstream_remote: downstream.peer_addr().ok(),
            downstream_local: Arc::clone(&self.listen_addr),
            session_table: self.stream_context.session_table.clone(),
            destination: Some(self.destination.clone()),
        };
        let io_copy = CopyBidirectional {
            downstream,
            upstream: upstream.stream,
            payload_crypto: proxy_chain.payload_crypto.clone(),
            speed_limiter: self.speed_limiter.clone(),
            conn_context,
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
impl loading::HandleConn for TcpAccessConnHandler {}
impl StreamServerHandleConn for TcpAccessConnHandler {
    #[instrument(skip(self, stream))]
    async fn handle_stream<Stream>(&self, stream: Stream)
    where
        Stream: OwnIoStream + HasIoAddr,
    {
        match self.proxy(stream).await {
            Ok(()) => (),
            Err(e) => warn!(?e, "Failed to proxy"),
        }
    }
}
