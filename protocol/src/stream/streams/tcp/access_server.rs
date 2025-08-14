use std::{collections::HashMap, io, sync::Arc};

use async_speed_limit::Limiter;
use common::{
    config::SharableConfig,
    loading,
    proto::{
        addr::{StreamAddr, StreamAddrStr},
        client::stream::{StreamEstablishError, establish},
        context::StreamContext,
        io_copy::stream::{ConnContext, CopyBidirectional},
        route::{StreamConnSelectorBuildContext, StreamConnSelectorBuilder, StreamRouteGroup},
    },
    route::ConnSelectorBuildError,
    stream::{HasIoAddr, OwnIoStream, StreamServerHandleConn},
};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tracing::{error, instrument, warn};

use super::proxy_server::TcpServer;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TcpAccessServerConfig {
    pub listen_addr: Arc<str>,
    pub destination: StreamAddrStr,
    pub conn_selector: SharableConfig<StreamConnSelectorBuilder>,
    pub speed_limit: Option<f64>,
}
impl TcpAccessServerConfig {
    pub fn into_builder(
        self,
        conn_selector: &HashMap<Arc<str>, StreamRouteGroup>,
        conn_selector_cx: StreamConnSelectorBuildContext<'_>,
        stream_context: StreamContext,
    ) -> Result<TcpAccessServerBuilder, BuildError> {
        let conn_selector = match self.conn_selector {
            SharableConfig::SharingKey(key) => conn_selector
                .get(&key)
                .ok_or_else(|| BuildError::ProxyGroupKeyNotFound(key.clone()))?
                .clone(),
            SharableConfig::Private(x) => x.build(conn_selector_cx.clone())?,
        };

        Ok(TcpAccessServerBuilder {
            listen_addr: self.listen_addr,
            destination: self.destination,
            conn_selector,
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
    ProxyGroup(#[from] ConnSelectorBuildError),
}

#[derive(Debug, Clone)]
pub struct TcpAccessServerBuilder {
    listen_addr: Arc<str>,
    destination: StreamAddrStr,
    conn_selector: StreamRouteGroup,
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
            self.conn_selector,
            self.destination.0,
            self.speed_limit,
            self.stream_context,
            Arc::clone(&self.listen_addr),
        ))
    }
}

#[derive(Debug)]
pub struct TcpAccessConnHandler {
    conn_selector: StreamRouteGroup,
    destination: StreamAddr,
    speed_limiter: Limiter,
    stream_context: StreamContext,
    listen_addr: Arc<str>,
}
impl TcpAccessConnHandler {
    pub fn new(
        conn_selector: StreamRouteGroup,
        destination: StreamAddr,
        speed_limit: f64,
        stream_context: StreamContext,
        listen_addr: Arc<str>,
    ) -> Self {
        Self {
            conn_selector,
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
        let (chain, payload_crypto) = match &self.conn_selector {
            common::route::ConnSelector::Empty => ([].into(), None),
            common::route::ConnSelector::Some(conn_selector1) => {
                let proxy_chain = conn_selector1.choose_chain();
                (
                    proxy_chain.chain.clone(),
                    proxy_chain.payload_crypto.clone(),
                )
            }
        };
        let upstream = establish(&chain, self.destination.clone(), &self.stream_context).await?;

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
            payload_crypto,
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
