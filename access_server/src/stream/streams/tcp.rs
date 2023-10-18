use std::{collections::HashMap, io, sync::Arc, time::SystemTime};

use async_speed_limit::Limiter;
use async_trait::async_trait;
use common::{
    config::SharableConfig,
    loading,
    stream::{
        addr::{StreamAddr, StreamAddrStr},
        copy_bidirectional_with_payload_crypto, get_metrics_from_copy_result,
        pool::Pool,
        proxy_table::StreamProxyTable,
        session_table::{Session, SessionTable},
        streams::tcp::TcpServer,
        tokio_io, IoAddr, IoStream, StreamMetrics, StreamServerHook,
    },
};
use proxy_client::stream::{establish, StreamEstablishError};
use scopeguard::defer;
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::net::ToSocketAddrs;
use tokio_util::sync::CancellationToken;
use tracing::{error, info, instrument, warn};

use crate::stream::proxy_table::{StreamProxyTableBuildError, StreamProxyTableBuilder};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TcpAccessServerConfig {
    pub listen_addr: Arc<str>,
    pub destination: StreamAddrStr,
    pub proxy_table: SharableConfig<StreamProxyTableBuilder>,
    pub speed_limit: Option<f64>,
}

impl TcpAccessServerConfig {
    pub fn into_builder(
        self,
        stream_pool: Pool,
        proxy_tables: &HashMap<Arc<str>, StreamProxyTable>,
        cancellation: CancellationToken,
        session_table: SessionTable,
    ) -> Result<TcpAccessServerBuilder, BuildError> {
        let proxy_table = match self.proxy_table {
            SharableConfig::SharingKey(key) => proxy_tables
                .get(&key)
                .ok_or_else(|| BuildError::ProxyTableKeyNotFound(key.clone()))?
                .clone(),
            SharableConfig::Private(x) => x.build(&stream_pool, cancellation)?,
        };

        Ok(TcpAccessServerBuilder {
            listen_addr: self.listen_addr,
            destination: self.destination,
            proxy_table,
            stream_pool,
            speed_limit: self.speed_limit.unwrap_or(f64::INFINITY),
            session_table,
        })
    }
}

#[derive(Debug, Error)]
pub enum BuildError {
    #[error("Proxy table key not found: {0}")]
    ProxyTableKeyNotFound(Arc<str>),
    #[error("{0}")]
    ProxyTable(#[from] StreamProxyTableBuildError),
}

#[derive(Debug, Clone)]
pub struct TcpAccessServerBuilder {
    listen_addr: Arc<str>,
    destination: StreamAddrStr,
    proxy_table: StreamProxyTable,
    stream_pool: Pool,
    speed_limit: f64,
    session_table: SessionTable,
}

#[async_trait]
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
            self.proxy_table,
            self.destination.0,
            self.stream_pool,
            self.speed_limit,
            self.session_table,
        ))
    }
}

#[derive(Debug)]
pub struct TcpAccess {
    proxy_table: StreamProxyTable,
    destination: StreamAddr,
    stream_pool: Pool,
    speed_limiter: Limiter,
    session_table: SessionTable,
}

impl TcpAccess {
    pub fn new(
        proxy_table: StreamProxyTable,
        destination: StreamAddr,
        stream_pool: Pool,
        speed_limit: f64,
        session_table: SessionTable,
    ) -> Self {
        Self {
            proxy_table,
            destination,
            stream_pool,
            speed_limiter: Limiter::new(speed_limit),
            session_table,
        }
    }

    pub async fn build(self, listen_addr: impl ToSocketAddrs) -> io::Result<TcpServer<Self>> {
        let tcp_listener = tokio::net::TcpListener::bind(listen_addr).await?;
        Ok(TcpServer::new(tcp_listener, self))
    }

    async fn proxy<S>(&self, downstream: S) -> Result<StreamMetrics, ProxyError>
    where
        S: IoStream + IoAddr,
    {
        let start = std::time::Instant::now();

        let downstream_addr = downstream.peer_addr().map_err(ProxyError::DownstreamAddr)?;

        let proxy_chain = self.proxy_table.choose_chain();
        let upstream = establish(
            &proxy_chain.chain,
            self.destination.clone(),
            &self.stream_pool,
        )
        .await?;

        let key = self.session_table.insert(Session {
            start: SystemTime::now(),
            destination: self.destination.clone(),
            upstream_local: upstream.stream.local_addr().unwrap(),
        });
        defer! { self.session_table.remove(key); }
        let res = copy_bidirectional_with_payload_crypto(
            downstream,
            upstream.stream,
            proxy_chain.payload_crypto.as_ref(),
            self.speed_limiter.clone(),
        )
        .await;

        let (metrics, res) = get_metrics_from_copy_result(
            start,
            upstream.addr,
            upstream.sock_addr,
            Some(downstream_addr),
            res,
        );
        match res {
            Ok(()) => Ok(metrics),
            Err(e) => Err(ProxyError::IoCopy { source: e, metrics }),
        }
    }
}

#[derive(Debug, Error)]
pub enum ProxyError {
    #[error("Failed to get downstream address: {0}")]
    DownstreamAddr(#[source] io::Error),
    #[error("Failed to establish proxy chain: {0}")]
    EstablishProxyChain(#[from] StreamEstablishError),
    #[error("Failed to copy data between streams: {source}, {metrics}")]
    IoCopy {
        #[source]
        source: tokio_io::CopyBiErrorKind,
        metrics: StreamMetrics,
    },
}

impl loading::Hook for TcpAccess {}

#[async_trait]
impl StreamServerHook for TcpAccess {
    #[instrument(skip(self, stream))]
    async fn handle_stream<S>(&self, stream: S)
    where
        S: IoStream + IoAddr,
    {
        match self.proxy(stream).await {
            Ok(metrics) => {
                info!(%metrics, "Proxy finished");
            }
            Err(ProxyError::IoCopy { source: e, metrics }) => {
                info!(?e, %metrics, "Proxy error");
            }
            Err(e) => warn!(?e, "Failed to proxy"),
        }
    }
}
