use std::{io, sync::Arc};

use async_trait::async_trait;
use common::{
    loading,
    stream::{
        addr::{StreamAddr, StreamAddrBuilder},
        copy_bidirectional_with_payload_crypto,
        pool::{Pool, PoolBuilder},
        proxy_table::{StreamProxyTable, StreamProxyTableBuilder},
        streams::tcp::TcpServer,
        tokio_io, FailedStreamMetrics, IoAddr, IoStream, StreamMetrics, StreamServerHook,
    },
};
use proxy_client::stream::{establish, StreamEstablishError};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::net::ToSocketAddrs;
use tracing::{error, info, instrument};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TcpProxyAccessBuilder {
    listen_addr: Arc<str>,
    proxy_table: StreamProxyTableBuilder,
    destination: StreamAddrBuilder,
    stream_pool: PoolBuilder,
}

#[async_trait]
impl loading::Builder for TcpProxyAccessBuilder {
    type Hook = TcpProxyAccess;
    type Server = TcpServer<Self::Hook>;

    async fn build_server(self) -> io::Result<TcpServer<TcpProxyAccess>> {
        let stream_pool = self.stream_pool.build();
        let access = TcpProxyAccess::new(
            self.proxy_table.build(),
            self.destination.build(),
            stream_pool,
        );
        let server = access.build(self.listen_addr.as_ref()).await?;
        Ok(server)
    }

    fn key(&self) -> &Arc<str> {
        &self.listen_addr
    }

    fn build_hook(self) -> io::Result<Self::Hook> {
        let stream_pool = self.stream_pool.build();
        Ok(TcpProxyAccess::new(
            self.proxy_table.build(),
            self.destination.build(),
            stream_pool,
        ))
    }
}

#[derive(Debug)]
pub struct TcpProxyAccess {
    proxy_table: StreamProxyTable,
    destination: StreamAddr,
    stream_pool: Pool,
}

impl TcpProxyAccess {
    pub fn new(proxy_table: StreamProxyTable, destination: StreamAddr, stream_pool: Pool) -> Self {
        Self {
            proxy_table,
            destination,
            stream_pool,
        }
    }

    pub async fn build(self, listen_addr: impl ToSocketAddrs) -> io::Result<TcpServer<Self>> {
        let tcp_listener = tokio::net::TcpListener::bind(listen_addr)
            .await
            .inspect_err(|e| error!(?e, "Failed to bind to listen address"))?;
        Ok(TcpServer::new(tcp_listener, self))
    }

    async fn proxy<S>(&self, downstream: S) -> Result<StreamMetrics, ProxyError>
    where
        S: IoStream + IoAddr,
    {
        let start = std::time::Instant::now();

        let downstream_addr = downstream.peer_addr().map_err(ProxyError::DownstreamAddr)?;

        let proxy_chain = self.proxy_table.choose_chain();
        let (upstream, upstream_addr, upstream_sock_addr) = establish(
            &proxy_chain.chain,
            self.destination.clone(),
            &self.stream_pool,
        )
        .await?;

        let res = copy_bidirectional_with_payload_crypto(
            downstream,
            upstream,
            proxy_chain.payload_crypto.as_ref(),
        )
        .await;
        let end = std::time::Instant::now();
        let (bytes_uplink, bytes_downlink) = res.map_err(|e| ProxyError::IoCopy {
            source: e,
            metrics: FailedStreamMetrics {
                start,
                end,
                upstream_addr: upstream_addr.clone(),
                upstream_sock_addr,
                downstream_addr: Some(downstream_addr),
            },
        })?;

        let metrics = StreamMetrics {
            start,
            end,
            bytes_uplink,
            bytes_downlink,
            upstream_addr,
            upstream_sock_addr,
            downstream_addr: Some(downstream_addr),
        };
        Ok(metrics)
    }
}

#[derive(Debug, Error)]
pub enum ProxyError {
    #[error("Failed to get downstream address")]
    DownstreamAddr(#[source] io::Error),
    #[error("Failed to establish proxy chain")]
    EstablishProxyChain(#[from] StreamEstablishError),
    #[error("Failed to copy data between streams")]
    IoCopy {
        #[source]
        source: tokio_io::CopyBiError,
        metrics: FailedStreamMetrics,
    },
}

impl loading::Hook for TcpProxyAccess {}

#[async_trait]
impl StreamServerHook for TcpProxyAccess {
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
            Err(e) => error!(?e, "Failed to proxy"),
        }
    }
}
