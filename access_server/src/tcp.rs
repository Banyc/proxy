use std::io;

use async_trait::async_trait;
use common::{
    crypto::XorCrypto,
    stream::{
        addr::{StreamAddr, StreamAddrBuilder},
        config::{StreamProxyConfig, StreamProxyConfigBuilder},
        copy_bidirectional_with_payload_crypto,
        pool::{Pool, PoolBuilder},
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
    listen_addr: String,
    proxies: Vec<StreamProxyConfigBuilder>,
    destination: StreamAddrBuilder,
    payload_xor_key: Option<Vec<u8>>,
    stream_pool: PoolBuilder,
}

impl TcpProxyAccessBuilder {
    pub async fn build(self) -> io::Result<TcpServer<TcpProxyAccess>> {
        let stream_pool = self.stream_pool.build();
        let access = TcpProxyAccess::new(
            self.proxies.into_iter().map(|x| x.build()).collect(),
            self.destination.build(),
            self.payload_xor_key.map(XorCrypto::new),
            stream_pool,
        );
        let server = access.build(self.listen_addr).await?;
        Ok(server)
    }
}

pub struct TcpProxyAccess {
    proxies: Vec<StreamProxyConfig>,
    destination: StreamAddr,
    payload_crypto: Option<XorCrypto>,
    stream_pool: Pool,
}

impl TcpProxyAccess {
    pub fn new(
        proxies: Vec<StreamProxyConfig>,
        destination: StreamAddr,
        payload_crypto: Option<XorCrypto>,
        stream_pool: Pool,
    ) -> Self {
        Self {
            proxies,
            destination,
            payload_crypto,
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

        let (upstream, upstream_addr, upstream_sock_addr) =
            establish(&self.proxies, self.destination.clone(), &self.stream_pool).await?;

        let res = copy_bidirectional_with_payload_crypto(
            downstream,
            upstream,
            self.payload_crypto.as_ref(),
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
