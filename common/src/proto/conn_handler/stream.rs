use std::{fmt, io, net::SocketAddr, sync::Arc};

use crate::{
    addr::ParseInternetAddrError,
    loading,
    proto::{
        addr::RouteAddr,
        conn::stream::ConnAndAddr,
        context::StreamRuntime,
        log::stream::IoCopyFinished,
        relay::stream::{ConnContext, CopyBidirectional},
        route_header::stream::{SteerError, read_route_header},
    },
    stream::{
        ConnParts, StreamServerHandleConn,
        pool::{ConnectError, connect_with_pool},
    },
};
use async_speed_limit::Limiter;
use serde::Deserialize;
use thiserror::Error;
use tracing::{info, instrument, warn};

pub struct StreamProxyFinished {
    pub io: IoCopyFinished,
    pub up: RouteAddr,
}

impl fmt::Display for StreamProxyFinished {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.io)?;
        write!(f, ",upstream:{}", self.up)?;
        Ok(())
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StreamProxyConnHandlerConfig {
    pub header_key: tokio_chacha20::config::ConfigBuilder,
    pub payload_key: Option<tokio_chacha20::config::ConfigBuilder>,
    #[serde(default)]
    pub allow_loopback: bool,
}

impl StreamProxyConnHandlerConfig {
    pub fn into_builder(
        self,
        stream_context: StreamRuntime,
        listen_addr: Arc<str>,
    ) -> StreamProxyConnHandlerBuilder {
        StreamProxyConnHandlerBuilder {
            header_key: self.header_key,
            payload_key: self.payload_key,
            allow_loopback: self.allow_loopback,
            stream_context,
            listen_addr,
        }
    }
}

#[derive(Debug, Clone)]
pub struct StreamProxyConnHandlerBuilder {
    pub header_key: tokio_chacha20::config::ConfigBuilder,
    pub payload_key: Option<tokio_chacha20::config::ConfigBuilder>,
    pub allow_loopback: bool,
    pub stream_context: StreamRuntime,
    pub listen_addr: Arc<str>,
}
impl StreamProxyConnHandlerBuilder {
    pub fn build(self) -> Result<StreamProxyConnHandler, StreamProxyServerBuildError> {
        let header_crypto = self
            .header_key
            .build()
            .map_err(|e| StreamProxyServerBuildError::HeaderCrypto(e.source.to_string()))?;
        let payload_crypto =
            match self.payload_key {
                Some(key) => Some(key.build().map_err(|e| {
                    StreamProxyServerBuildError::PayloadCrypto(e.source.to_string())
                })?),
                None => None,
            };
        Ok(StreamProxyConnHandler::new(
            header_crypto,
            payload_crypto,
            self.stream_context,
            Arc::clone(&self.listen_addr),
            self.allow_loopback,
        ))
    }
}
#[derive(Debug, Error)]
pub enum StreamProxyServerBuildError {
    #[error("HeaderCrypto: {0}")]
    HeaderCrypto(String),
    #[error("PayloadCrypto: {0}")]
    PayloadCrypto(String),
    #[error("Stream pool: {0}")]
    StreamPool(#[from] ParseInternetAddrError),
}

#[derive(Debug)]
pub struct StreamProxyConnHandler {
    acceptor: StreamProxyAcceptor,
    payload_crypto: Option<tokio_chacha20::config::Config>,
    stream_context: StreamRuntime,
    listen_addr: Arc<str>,
}
impl StreamProxyConnHandler {
    pub fn new(
        header_crypto: tokio_chacha20::config::Config,
        payload_crypto: Option<tokio_chacha20::config::Config>,
        stream_context: StreamRuntime,
        listen_addr: Arc<str>,
        allow_loopback: bool,
    ) -> Self {
        Self {
            acceptor: StreamProxyAcceptor::new(
                header_crypto,
                stream_context.clone(),
                allow_loopback,
            ),
            payload_crypto,
            stream_context,
            listen_addr,
        }
    }

    #[instrument(skip_all)]
    async fn proxy<Downstream>(
        &self,
        mut downstream: Downstream,
    ) -> Result<ProxyResult, StreamProxyServerError>
    where
        Downstream: ConnParts + std::fmt::Debug,
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
        let up = upstream.addr.clone();
        let conn_context = ConnContext {
            start: (std::time::Instant::now(), std::time::SystemTime::now()),
            upstream_remote: upstream.addr,
            upstream_remote_sock: upstream.sock_addr,
            upstream_local: upstream.stream.local_addr().ok(),
            downstream_remote: downstream.peer_addr().ok(),
            downstream_local: Arc::clone(&self.listen_addr),
            session_table: self.stream_context.session_table.clone(),
            destination: None,
        };
        let io_copy = CopyBidirectional {
            downstream,
            upstream: upstream.stream,
            payload_crypto: self.payload_crypto.clone(),
            speed_limiter: Limiter::new(f64::INFINITY),
            conn_context,
            retention: self.stream_context.retention.clone(),
        }
        .serve_as_proxy_server();
        let (io, res) = io_copy.await;
        let log = StreamProxyFinished { io, up };
        match &res {
            Ok(()) => crate::info_println!("Stream: Finished {log}"),
            Err(err) => crate::info_println!("Stream: Error {log}: {err}"),
        }
        Ok(ProxyResult::IoCopy)
    }
}
impl loading::HandleConn for StreamProxyConnHandler {}
impl StreamServerHandleConn for StreamProxyConnHandler {
    #[instrument(skip_all)]
    async fn handle_stream<Stream>(&self, stream: Stream)
    where
        Stream: ConnParts + std::fmt::Debug,
    {
        let local_addr = stream.local_addr().ok();
        let peer_addr = stream.peer_addr().ok();
        match self.proxy(stream).await {
            Ok(ProxyResult::IoCopy) => (),
            Ok(ProxyResult::Echo) => info!(?local_addr, ?peer_addr, "Echo finished"),
            Err(e) => {
                let upstream_addr = e.upstream_addr();
                warn!(
                    event = "stream_proxy_failed",
                    ?e,
                    dn = ?peer_addr,
                    up = ?upstream_addr,
                    dn_local = ?local_addr,
                    listener = %self.listen_addr,
                    "Proxy error"
                );
            }
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
    stream_context: StreamRuntime,
    allow_loopback: bool,
}
impl StreamProxyAcceptor {
    pub fn new(
        crypto: tokio_chacha20::config::Config,
        stream_context: StreamRuntime,
        allow_loopback: bool,
    ) -> Self {
        Self {
            crypto,
            stream_context,
            allow_loopback,
        }
    }

    #[instrument(skip_all)]
    async fn establish<Downstream>(
        &self,
        downstream: &mut Downstream,
    ) -> Result<Option<ConnAndAddr>, StreamProxyAcceptorError>
    where
        Downstream: ConnParts + std::fmt::Debug,
    {
        let addr = match read_route_header(
            downstream,
            &self.crypto,
            &self.stream_context.replay_validator,
        )
        .await?
        {
            Some(addr) => addr,
            None => return Ok(None),
        };
        let (upstream, sock_addr) = connect_with_pool(
            &addr,
            &self.stream_context,
            self.allow_loopback,
            crate::STREAM_IO_TIMEOUT,
        )
        .await
        .map_err(|e| {
            let downstream_addr = downstream.peer_addr().ok();
            StreamProxyAcceptorError::ConnectUpstream {
                source: e,
                downstream_addr,
                upstream_addr: addr.clone(),
            }
        })?;
        Ok(Some(ConnAndAddr {
            stream: upstream,
            addr,
            sock_addr,
        }))
    }
}

#[derive(Debug, Error)]
pub enum StreamProxyServerError {
    #[error("Failed to get downstream address: {0}")]
    DownstreamAddr(#[source] io::Error),
    #[error("Failed to establish proxy chain: {0}")]
    EstablishProxyChain(#[from] StreamProxyAcceptorError),
}
impl StreamProxyServerError {
    fn upstream_addr(&self) -> Option<&RouteAddr> {
        match self {
            Self::EstablishProxyChain(error) => error.upstream_addr(),
            Self::DownstreamAddr(_) => None,
        }
    }
}

#[derive(Debug, Error)]
pub enum StreamProxyAcceptorError {
    #[error("Steer error: {0}")]
    Steer(#[from] SteerError),
    #[error("Failed to connect to upstream {upstream_addr}: {source}, {downstream_addr:?}")]
    ConnectUpstream {
        #[source]
        source: ConnectError,
        downstream_addr: Option<SocketAddr>,
        upstream_addr: RouteAddr,
    },
}
impl StreamProxyAcceptorError {
    fn upstream_addr(&self) -> Option<&RouteAddr> {
        match self {
            Self::ConnectUpstream { upstream_addr, .. } => Some(upstream_addr),
            Self::Steer(_) => None,
        }
    }
}
