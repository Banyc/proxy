use std::{io, net::SocketAddr, sync::Arc, time::Duration};

use async_trait::async_trait;
use common::{
    crypto::{XorCrypto, XorCryptoCursor},
    header::{
        codec::{timed_read_header_async, timed_write_header_async, CodecError},
        heartbeat::{self, HeartbeatError},
        route::RouteResponse,
    },
    loading,
    stream::{
        addr::StreamAddr,
        connect_with_pool, copy_bidirectional_with_payload_crypto,
        header::StreamRequestHeader,
        pool::{Pool, PoolBuilder},
        tokio_io, ConnectError, CreatedStream, FailedStreamMetrics, IoAddr, IoStream,
        StreamMetrics, StreamServerHook,
    },
};
use serde::Deserialize;
use thiserror::Error;
use tokio::io::AsyncWriteExt;
use tracing::{error, info, instrument, warn};

pub mod kcp;
pub mod tcp;

const IO_TIMEOUT: Duration = Duration::from_secs(60);

#[derive(Debug, Clone, PartialEq, Eq, Hash, Deserialize)]
pub struct StreamProxyBuilder {
    pub header_xor_key: Arc<[u8]>,
    pub payload_xor_key: Option<Arc<[u8]>>,
    pub stream_pool: PoolBuilder,
}

impl StreamProxyBuilder {
    pub fn build(self) -> StreamProxy {
        let header_crypto = XorCrypto::new(self.header_xor_key);
        let payload_crypto = self.payload_xor_key.map(XorCrypto::new);
        let stream_pool = self.stream_pool.build();
        StreamProxy::new(header_crypto, payload_crypto, stream_pool)
    }
}

#[derive(Debug)]
pub struct StreamProxy {
    acceptor: StreamProxyAcceptor,
    payload_crypto: Option<XorCrypto>,
}

impl StreamProxy {
    pub fn new(
        header_crypto: XorCrypto,
        payload_crypto: Option<XorCrypto>,
        stream_pool: Pool,
    ) -> Self {
        Self {
            acceptor: StreamProxyAcceptor::new(header_crypto, stream_pool),
            payload_crypto,
        }
    }

    #[instrument(skip(self))]
    async fn proxy<S>(
        &self,
        mut downstream: S,
    ) -> Result<Option<StreamMetrics>, StreamProxyServerError>
    where
        S: IoStream + IoAddr + std::fmt::Debug,
    {
        let start = std::time::Instant::now();

        let downstream_addr = downstream
            .peer_addr()
            .map_err(StreamProxyServerError::DownstreamAddr)?;

        // Establish proxy chain
        let (upstream, upstream_addr, upstream_sock_addr) =
            match self.acceptor.establish(&mut downstream).await {
                Ok(Some(upstream)) => upstream,
                Ok(None) => return Ok(None),
                Err(e) => {
                    // self.handle_proxy_error(&mut downstream, e).await;
                    return Err(StreamProxyServerError::EstablishProxyChain(e));
                }
            };

        // Copy data
        let res = copy_bidirectional_with_payload_crypto(
            downstream,
            upstream,
            self.payload_crypto.as_ref(),
            f64::INFINITY,
        )
        .await;
        let end = std::time::Instant::now();
        let (bytes_uplink, bytes_downlink) = res.map_err(|e| StreamProxyServerError::IoCopy {
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
        Ok(Some(metrics))
    }

    // #[instrument(skip(self, e))]
    // async fn handle_proxy_error<S>(&self, stream: &mut S, e: ProxyProtocolError)
    // where
    //     S: IoStream + IoAddr + std::fmt::Debug,
    // {
    //     error!(?e, "Connection closed with error");
    //     let _ = self
    //         .acceptor
    //         .respond_with_error(stream, e)
    //         .await
    //         .inspect_err(|e| {
    //             let peer_addr = stream.peer_addr().ok();
    //             error!(
    //                 ?e,
    //                 ?peer_addr,
    //                 "Failed to respond with error to downstream after error"
    //             )
    //         });
    // }
}

impl loading::Hook for StreamProxy {}

#[async_trait]
impl StreamServerHook for StreamProxy {
    #[instrument(skip(self))]
    async fn handle_stream<S>(&self, stream: S)
    where
        S: IoStream + IoAddr + std::fmt::Debug,
    {
        match self.proxy(stream).await {
            Ok(Some(metrics)) => info!(%metrics, "Proxy finished"),
            Ok(None) => info!("Echo finished"),
            Err(StreamProxyServerError::IoCopy { source: e, metrics }) => {
                info!(?e, %metrics, "Proxy error");
            }
            Err(e) => warn!(?e, "Proxy error"),
        }
    }
}

#[derive(Debug)]
pub struct StreamProxyAcceptor {
    crypto: XorCrypto,
    stream_pool: Pool,
}

impl StreamProxyAcceptor {
    pub fn new(crypto: XorCrypto, stream_pool: Pool) -> Self {
        Self {
            crypto,
            stream_pool,
        }
    }

    #[instrument(skip(self))]
    async fn establish<S>(
        &self,
        downstream: &mut S,
    ) -> Result<Option<(CreatedStream, StreamAddr, SocketAddr)>, StreamProxyAcceptorError>
    where
        S: IoStream + IoAddr + std::fmt::Debug,
    {
        // Wait for heartbeat upgrade
        heartbeat::wait_upgrade(downstream, IO_TIMEOUT)
            .await
            .map_err(|e| {
                let downstream_addr = downstream.peer_addr().ok();
                StreamProxyAcceptorError::ReadHeartbeatUpgrade {
                    source: e,
                    downstream_addr,
                }
            })?;

        // Decode header
        let mut read_crypto_cursor = XorCryptoCursor::new(&self.crypto);
        let header: StreamRequestHeader =
            timed_read_header_async(downstream, &mut read_crypto_cursor, IO_TIMEOUT)
                .await
                .map_err(|e| {
                    let downstream_addr = downstream.peer_addr().ok();
                    StreamProxyAcceptorError::ReadStreamRequestHeader {
                        source: e,
                        downstream_addr,
                    }
                })?;

        // Echo
        let addr = match header.upstream {
            Some(upstream) => upstream,
            None => {
                let resp = RouteResponse { result: Ok(()) };
                let mut write_crypto_cursor = XorCryptoCursor::new(&self.crypto);
                timed_write_header_async(downstream, &resp, &mut write_crypto_cursor, IO_TIMEOUT)
                    .await
                    .map_err(|e| {
                        let downstream_addr = downstream.peer_addr().ok();
                        StreamProxyAcceptorError::WriteEchoResponse {
                            source: e,
                            downstream_addr,
                        }
                    })?;
                let _ = tokio::time::timeout(IO_TIMEOUT, downstream.flush()).await;

                return Ok(None);
            }
        };

        // Connect to upstream
        let (upstream, sock_addr) = connect_with_pool(&addr, &self.stream_pool, false, IO_TIMEOUT)
            .await
            .map_err(|e| {
                let downstream_addr = downstream.peer_addr().ok();
                StreamProxyAcceptorError::ConnectUpstream {
                    source: e,
                    downstream_addr,
                }
            })?;

        // // Write Ok response
        // let resp = ResponseHeader { result: Ok(()) };
        // let mut write_crypto_cursor = XorCryptoCursor::new(&self.crypto);
        // write_header_async(downstream, &resp, &mut write_crypto_cursor)
        //     .await
        //     .inspect_err(
        //         |e| error!(?e, ?header.upstream, "Failed to write response to downstream"),
        //     )?;

        // Return upstream
        Ok(Some((upstream, addr, sock_addr)))
    }

    // #[instrument(skip(self))]
    // async fn respond_with_error<S>(
    //     &self,
    //     stream: &mut S,
    //     error: ProxyProtocolError,
    // ) -> Result<(), ProxyProtocolError>
    // where
    //     S: IoStream + IoAddr + std::fmt::Debug,
    // {
    //     let local_addr = stream
    //         .local_addr()
    //         .inspect_err(|e| error!(?e, "Failed to get local address"))?;

    //     // Respond with error
    //     let resp = error.into_response_header(local_addr.into());
    //     let mut crypto_cursor = XorCryptoCursor::new(&self.crypto);
    //     write_header_async(stream, &resp, &mut crypto_cursor)
    //         .await
    //         .inspect_err(|e| {
    //             let peer_addr = stream.peer_addr().ok();
    //             error!(
    //                 ?e,
    //                 ?peer_addr,
    //                 "Failed to write response to downstream after error"
    //             )
    //         })?;

    //     Ok(())
    // }
}

#[derive(Debug, Error)]
pub enum StreamProxyServerError {
    #[error("Failed to get downstream address")]
    DownstreamAddr(#[source] io::Error),
    #[error("Failed to establish proxy chain")]
    EstablishProxyChain(#[from] StreamProxyAcceptorError),
    #[error("Failed to copy data between streams")]
    IoCopy {
        #[source]
        source: tokio_io::CopyBiError,
        metrics: FailedStreamMetrics,
    },
}

#[derive(Debug, Error)]
pub enum StreamProxyAcceptorError {
    #[error("Failed to read heartbeat header from downstream")]
    ReadHeartbeatUpgrade {
        #[source]
        source: HeartbeatError,
        downstream_addr: Option<SocketAddr>,
    },
    #[error("Failed to read stream request header from downstream")]
    ReadStreamRequestHeader {
        #[source]
        source: CodecError,
        downstream_addr: Option<SocketAddr>,
    },
    #[error("Failed to write echo response to downstream")]
    WriteEchoResponse {
        #[source]
        source: CodecError,
        downstream_addr: Option<SocketAddr>,
    },
    #[error("Failed to connect to upstream")]
    ConnectUpstream {
        #[source]
        source: ConnectError,
        downstream_addr: Option<SocketAddr>,
    },
}
