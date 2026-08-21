use std::{collections::HashMap, fmt, io, sync::Arc};

use async_speed_limit::Limiter;
use common::{
    config::SharableConfig,
    loading,
    proto::{
        addr::{RouteAddr, RouteAddrStr},
        client::stream::{StreamEstablishError, establish},
        context::StreamRuntime,
        log::stream::IoCopyFinished,
        relay::stream::{ConnContext, CopyBidirectional},
    },
    route::{
        ProbeFutures, Registries, RouteSelector, RouteSelectorBuildError, RouteSelectorBuilder,
    },
    stream::{HasIoAddr, OwnedIoStream, StreamServerHandleConn},
};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tracing::{instrument, warn};

pub struct TcpAccessLog {
    pub io: IoCopyFinished,
    pub dst: RouteAddr,
}

impl fmt::Display for TcpAccessLog {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.io)?;
        write!(f, ",dst:{}", self.dst)?;
        Ok(())
    }
}

use super::listener::TcpServer;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TcpAccessServerConfig {
    pub listen_addr: Arc<str>,
    pub destination: RouteAddrStr,
    pub conn_selector: SharableConfig<RouteSelectorBuilder>,
    pub speed_limit: Option<f64>,
}
impl TcpAccessServerConfig {
    pub fn into_builder(
        self,
        conn_selector: &HashMap<Arc<str>, RouteSelector>,
        registries: &Registries<'_>,
        stream_runtime: StreamRuntime,
        probes: &mut ProbeFutures,
    ) -> Result<TcpAccessServerBuilder, TcpAccessBuildError> {
        let conn_selector = match self.conn_selector {
            SharableConfig::SharingKey(key) => conn_selector
                .get(&key)
                .ok_or_else(|| TcpAccessBuildError::ProxyGroupKeyNotFound(key.clone()))?
                .clone(),
            SharableConfig::Private(x) => x.resolve(registries, probes)?,
        };

        Ok(TcpAccessServerBuilder {
            listen_addr: self.listen_addr,
            destination: self.destination,
            conn_selector,
            speed_limit: self.speed_limit.unwrap_or(f64::INFINITY),
            stream_runtime,
        })
    }
}
#[derive(Debug, Error)]
pub enum TcpAccessBuildError {
    #[error("Proxy group key not found: {0}")]
    ProxyGroupKeyNotFound(Arc<str>),
    #[error("{0}")]
    ProxyGroup(#[from] RouteSelectorBuildError),
}

#[derive(Debug, Clone)]
pub struct TcpAccessServerBuilder {
    listen_addr: Arc<str>,
    destination: RouteAddrStr,
    conn_selector: RouteSelector,
    speed_limit: f64,
    stream_runtime: StreamRuntime,
}
impl loading::Build for TcpAccessServerBuilder {
    type ConnHandler = TcpAccessConnHandler;
    type Server = TcpServer<Self::ConnHandler>;
    type Err = io::Error;

    async fn build_server(self) -> Result<Self::Server, Self::Err> {
        let listen_addr = self.listen_addr.clone();
        let session_spawner = self.stream_runtime.session_spawner.clone();
        let access = self.build_conn_handler()?;
        let tcp_listener = tokio::net::TcpListener::bind(listen_addr.as_ref()).await?;
        let server = TcpServer::new(tcp_listener, access, session_spawner);
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
            self.stream_runtime,
            Arc::clone(&self.listen_addr),
        ))
    }
}

#[derive(Debug)]
pub struct TcpAccessConnHandler {
    conn_selector: RouteSelector,
    destination: RouteAddr,
    speed_limiter: Limiter,
    stream_runtime: StreamRuntime,
    listen_addr: Arc<str>,
}
impl TcpAccessConnHandler {
    pub fn new(
        conn_selector: RouteSelector,
        destination: RouteAddr,
        speed_limit: f64,
        stream_runtime: StreamRuntime,
        listen_addr: Arc<str>,
    ) -> Self {
        Self {
            conn_selector,
            destination,
            speed_limiter: Limiter::new(speed_limit),
            stream_runtime,
            listen_addr,
        }
    }

    async fn proxy<Downstream>(&self, downstream: Downstream) -> Result<(), TcpAccessProxyError>
    where
        Downstream: OwnedIoStream + HasIoAddr,
    {
        let chain = match &self.conn_selector {
            common::route::RouteSelector::Empty => [].into(),
            common::route::RouteSelector::Some(non_empty_conn_selector) => {
                non_empty_conn_selector.choose_chain().chain.clone()
            }
        };
        let upstream = establish(&chain, self.destination.clone(), &self.stream_runtime).await?;
        let conn_context = ConnContext {
            start: (std::time::Instant::now(), std::time::SystemTime::now()),
            upstream_remote: upstream.addr,
            upstream_remote_sock: upstream.sock_addr,
            upstream_local: upstream.stream.local_addr().ok(),
            downstream_remote: downstream.peer_addr().ok(),
            downstream_local: Arc::clone(&self.listen_addr),
            session_table: self.stream_runtime.session_table.clone(),
            destination: Some(self.destination.clone()),
        };
        let dst = self.destination.clone();
        let retention = self.stream_runtime.retention.clone();
        let io_copy = CopyBidirectional {
            downstream,
            upstream: upstream.stream,
            payload_crypto: None,
            speed_limiter: self.speed_limiter.clone(),
            conn_context,
            retention,
        }
        .serve_as_access_server();
        let (io, res) = io_copy.await;
        let log = TcpAccessLog { io, dst };
        match &res {
            Ok(()) => common::info_println!("TCP: Finished {log}"),
            Err(err) => common::info_println!("TCP: Error {log}: {err}"),
        }
        Ok(())
    }
}
#[derive(Debug, Error)]
pub enum TcpAccessProxyError {
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
        Stream: OwnedIoStream + HasIoAddr,
    {
        match self.proxy(stream).await {
            Ok(()) => (),
            Err(e) => warn!(?e, "Failed to proxy"),
        }
    }
}
