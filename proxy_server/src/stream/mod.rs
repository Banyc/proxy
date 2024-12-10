use std::{io, net::SocketAddr, time::Duration};

use async_speed_limit::Limiter;
use common::{
    addr::ParseInternetAddrError,
    loading,
    stream::{
        io_copy::{CopyBidirectional, LogContext},
        pool::connect_with_pool,
        steer::{steer, SteerError},
        IoAddr, IoStream, StreamServerHandleConn,
    },
};
use protocol::stream::{
    connection::ConnAndAddr, context::ConcreteStreamContext, pool::ConcreteConnectError,
};
use serde::Deserialize;
use thiserror::Error;
use tracing::{error, info, instrument, warn};

pub mod kcp;
pub mod mptcp;
pub mod rtp;
pub mod tcp;

const IO_TIMEOUT: Duration = Duration::from_secs(60);

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StreamProxyServerConfig {
    pub header_key: tokio_chacha20::config::ConfigBuilder,
    pub payload_key: Option<tokio_chacha20::config::ConfigBuilder>,
}

impl StreamProxyServerConfig {
    pub fn into_builder(
        self,
        stream_context: ConcreteStreamContext,
    ) -> StreamProxyConnHandlerBuilder {
        StreamProxyConnHandlerBuilder {
            header_key: self.header_key,
            payload_key: self.payload_key,
            stream_context,
        }
    }
}

#[derive(Debug, Clone)]
pub struct StreamProxyConnHandlerBuilder {
    pub header_key: tokio_chacha20::config::ConfigBuilder,
    pub payload_key: Option<tokio_chacha20::config::ConfigBuilder>,
    pub stream_context: ConcreteStreamContext,
}
impl StreamProxyConnHandlerBuilder {
    pub fn build(self) -> Result<StreamProxyConnHandler, StreamProxyServerBuildError> {
        let header_crypto = self
            .header_key
            .build()
            .map_err(StreamProxyServerBuildError::HeaderCrypto)?;
        let payload_crypto = match self.payload_key {
            Some(key) => Some(
                key.build()
                    .map_err(StreamProxyServerBuildError::PayloadCrypto)?,
            ),
            None => None,
        };
        Ok(StreamProxyConnHandler::new(
            header_crypto,
            payload_crypto,
            self.stream_context,
        ))
    }
}
#[derive(Debug, Error)]
pub enum StreamProxyServerBuildError {
    #[error("HeaderCrypto: {0}")]
    HeaderCrypto(#[source] tokio_chacha20::config::ConfigBuildError),
    #[error("PayloadCrypto: {0}")]
    PayloadCrypto(#[source] tokio_chacha20::config::ConfigBuildError),
    #[error("Stream pool: {0}")]
    StreamPool(#[from] ParseInternetAddrError),
}

#[derive(Debug)]
pub struct StreamProxyConnHandler {
    acceptor: StreamProxyAcceptor,
    payload_crypto: Option<tokio_chacha20::config::Config>,
    stream_context: ConcreteStreamContext,
}
impl StreamProxyConnHandler {
    pub fn new(
        header_crypto: tokio_chacha20::config::Config,
        payload_crypto: Option<tokio_chacha20::config::Config>,
        stream_context: ConcreteStreamContext,
    ) -> Self {
        Self {
            acceptor: StreamProxyAcceptor::new(header_crypto, stream_context.clone()),
            payload_crypto,
            stream_context,
        }
    }

    #[instrument(skip(self))]
    async fn proxy<S>(&self, mut downstream: S) -> Result<ProxyResult, StreamProxyServerError>
    where
        S: IoStream + IoAddr + std::fmt::Debug,
    {
        // Establish proxy chain
        let upstream = match self.acceptor.establish(&mut downstream).await {
            Ok(Some(upstream)) => upstream,
            Ok(None) => return Ok(ProxyResult::Echo),
            Err(e) => {
                // self.handle_proxy_error(&mut downstream, e).await;
                return Err(StreamProxyServerError::EstablishProxyChain(e));
            }
        };

        // Copy data
        let log_context = LogContext {
            start: (std::time::Instant::now(), std::time::SystemTime::now()),
            upstream_addr: upstream.addr,
            upstream_sock_addr: upstream.sock_addr,
            downstream_addr: downstream.peer_addr().ok(),
            upstream_local: upstream.stream.local_addr().ok(),
            session_table: self.stream_context.session_table.clone(),
            destination: None,
        };
        let io_copy = CopyBidirectional {
            downstream,
            upstream: upstream.stream,
            payload_crypto: self.payload_crypto.clone(),
            speed_limiter: Limiter::new(f64::INFINITY),
            log_context,
        }
        .serve_as_proxy_server("Stream");
        tokio::spawn(async move {
            let _ = io_copy.await;
        });
        Ok(ProxyResult::IoCopy)
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
impl loading::HandleConn for StreamProxyConnHandler {}
impl StreamServerHandleConn for StreamProxyConnHandler {
    #[instrument(skip(self))]
    async fn handle_stream<S>(&self, stream: S)
    where
        S: IoStream + IoAddr + std::fmt::Debug,
    {
        match self.proxy(stream).await {
            Ok(ProxyResult::IoCopy) => (),
            Ok(ProxyResult::Echo) => info!("Echo finished"),
            Err(e) => warn!(?e, "Proxy error"),
        }
    }
}

pub enum ProxyResult {
    Echo,
    IoCopy,
}

#[derive(Debug)]
pub struct StreamProxyAcceptor {
    crypto: tokio_chacha20::config::Config,
    stream_context: ConcreteStreamContext,
}
impl StreamProxyAcceptor {
    pub fn new(
        crypto: tokio_chacha20::config::Config,
        stream_context: ConcreteStreamContext,
    ) -> Self {
        Self {
            crypto,
            stream_context,
        }
    }

    #[instrument(skip(self))]
    async fn establish<S>(
        &self,
        downstream: &mut S,
    ) -> Result<Option<ConnAndAddr>, StreamProxyAcceptorError>
    where
        S: IoStream + IoAddr + std::fmt::Debug,
    {
        let addr = match steer(downstream, &self.crypto).await? {
            Some(addr) => addr,
            None => return Ok(None), // Echo
        };

        // Connect to upstream
        let (upstream, sock_addr) =
            connect_with_pool(&addr, &self.stream_context, false, IO_TIMEOUT)
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
        Ok(Some(ConnAndAddr {
            stream: upstream,
            addr,
            sock_addr,
        }))
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
    #[error("Failed to get downstream address: {0}")]
    DownstreamAddr(#[source] io::Error),
    #[error("Failed to establish proxy chain: {0}")]
    EstablishProxyChain(#[from] StreamProxyAcceptorError),
}

#[derive(Debug, Error)]
pub enum StreamProxyAcceptorError {
    #[error("Steer error: {0}")]
    Steer(#[from] SteerError),
    #[error("Failed to connect to upstream: {source}, {downstream_addr:?}")]
    ConnectUpstream {
        #[source]
        source: ConcreteConnectError,
        downstream_addr: Option<SocketAddr>,
    },
}
