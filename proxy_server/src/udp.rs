use std::{io, net::SocketAddr, sync::Arc};

use async_speed_limit::Limiter;
use async_trait::async_trait;
use common::{
    addr::{any_addr, InternetAddr},
    crypto::{XorCrypto, XorCryptoBuildError, XorCryptoBuilder, XorCryptoCursor},
    header::{
        codec::write_header,
        route::{RouteErrorKind, RouteResponse},
    },
    loading,
    udp::{
        io_copy::{copy_bidirectional, CopyBiError, DownstreamParts, UpstreamParts},
        respond::respond_with_error,
        steer::steer,
        Flow, FlowMetrics, Packet, UdpDownstreamWriter, UdpServer, UdpServerHook, UpstreamAddr,
    },
};
use serde::Deserialize;
use thiserror::Error;
use tokio::{
    net::{ToSocketAddrs, UdpSocket},
    sync::mpsc,
};
use tracing::{error, info, instrument, trace, warn};

use crate::ListenerBindError;

#[derive(Debug, Clone, PartialEq, Eq, Hash, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UdpProxyServerBuilder {
    pub listen_addr: Arc<str>,
    pub header_xor_key: XorCryptoBuilder,
    pub payload_xor_key: Option<XorCryptoBuilder>,
}

#[async_trait]
impl loading::Builder for UdpProxyServerBuilder {
    type Hook = UdpProxy;
    type Server = UdpServer<Self::Hook>;
    type Err = UdpProxyServerBuildError;

    async fn build_server(self) -> Result<Self::Server, Self::Err> {
        let listen_addr = Arc::clone(&self.listen_addr);
        let udp_proxy = self.build_hook()?;
        let server = udp_proxy.build(listen_addr.as_ref()).await?;
        Ok(server)
    }

    fn build_hook(self) -> Result<Self::Hook, Self::Err> {
        let header_crypto = self
            .header_xor_key
            .build()
            .map_err(UdpProxyBuildError::HeaderCrypto)?;
        let payload_crypto = match self.payload_xor_key {
            Some(payload_crypto) => Some(
                payload_crypto
                    .build()
                    .map_err(UdpProxyBuildError::HeaderCrypto)?,
            ),
            None => None,
        };
        Ok(UdpProxy::new(header_crypto, payload_crypto))
    }

    fn key(&self) -> &Arc<str> {
        &self.listen_addr
    }
}

#[derive(Debug, Error)]
pub enum UdpProxyBuildError {
    #[error("HeaderCrypto: {0}")]
    HeaderCrypto(#[source] XorCryptoBuildError),
    #[error("PayloadCrypto: {0}")]
    PayloadCrypto(#[source] XorCryptoBuildError),
}

#[derive(Debug, Error)]
pub enum UdpProxyServerBuildError {
    #[error("{0}")]
    Hook(#[from] UdpProxyBuildError),
    #[error("{0}")]
    Server(#[from] ListenerBindError),
}

#[derive(Debug)]
pub struct UdpProxy {
    header_crypto: XorCrypto,
    payload_crypto: Option<XorCrypto>,
}

impl UdpProxy {
    pub fn new(header_crypto: XorCrypto, payload_crypto: Option<XorCrypto>) -> Self {
        Self {
            header_crypto,
            payload_crypto,
        }
    }

    pub async fn build(
        self,
        listen_addr: impl ToSocketAddrs,
    ) -> Result<UdpServer<Self>, ListenerBindError> {
        let listener = UdpSocket::bind(listen_addr)
            .await
            .map_err(ListenerBindError)?;
        Ok(UdpServer::new(listener, self))
    }

    #[instrument(skip(self, rx, downstream_writer))]
    async fn proxy(
        &self,
        rx: mpsc::Receiver<Packet>,
        flow: Flow,
        downstream_writer: UdpDownstreamWriter,
    ) -> Result<FlowMetrics, ProxyError> {
        // Prevent connections to localhost
        let resolved_upstream =
            flow.upstream
                .0
                .to_socket_addr()
                .await
                .map_err(|e| ProxyError::Resolve {
                    source: e,
                    addr: flow.upstream.0.clone(),
                })?;
        if resolved_upstream.ip().is_loopback() {
            return Err(ProxyError::Loopback {
                addr: flow.upstream.0,
                sock_addr: resolved_upstream,
            });
        }

        // Connect to upstream
        let any_addr = any_addr(&resolved_upstream.ip());
        let upstream = UdpSocket::bind(any_addr)
            .await
            .map_err(ProxyError::ClientBindAny)?;
        upstream
            .connect(resolved_upstream)
            .await
            .map_err(|e| ProxyError::ConnectUpstream {
                source: e,
                addr: flow.upstream.0.clone(),
                sock_addr: resolved_upstream,
            })?;
        let upstream = Arc::new(upstream);

        let response_header = {
            // Write header
            let mut wtr = Vec::new();
            let header = RouteResponse { result: Ok(()) };
            let mut crypto_cursor = XorCryptoCursor::new(&self.header_crypto);
            write_header(&mut wtr, &header, &mut crypto_cursor).unwrap();
            wtr.into()
        };

        let metrics = copy_bidirectional(
            flow.clone(),
            UpstreamParts {
                read: upstream.clone(),
                write: upstream,
            },
            DownstreamParts {
                rx,
                write: downstream_writer,
            },
            Limiter::new(f64::INFINITY),
            self.payload_crypto.clone(),
            Some(response_header),
        )
        .await
        .map_err(|e| ProxyError::Copy {
            source: e,
            addr: flow.upstream.0,
            sock_addr: resolved_upstream,
        })?;
        Ok(metrics)
    }

    async fn handle_proxy_result(
        &self,
        downstream_writer: &UdpDownstreamWriter,
        res: Result<FlowMetrics, ProxyError>,
    ) {
        match res {
            Ok(metrics) => {
                info!(%metrics, "Proxy finished");
                // No response
            }
            Err(e) => {
                let peer_addr = downstream_writer.peer_addr();
                warn!(?e, ?peer_addr, "Proxy failed");
                let kind = error_kind_from_proxy_error(e);
                if let Err(e) =
                    respond_with_error(downstream_writer, kind, &self.header_crypto).await
                {
                    trace!(?e, ?peer_addr, "Failed to respond with error");
                }
            }
        }
    }
}

#[derive(Debug, Error)]
pub enum ProxyError {
    #[error("Failed to resolve upstream address: {source}, {addr}")]
    Resolve {
        #[source]
        source: io::Error,
        addr: InternetAddr,
    },
    #[error("Refused to connect to a loopback address: {addr}, {sock_addr}")]
    Loopback {
        addr: InternetAddr,
        sock_addr: SocketAddr,
    },
    #[error("Failed to created a client socket: {0}")]
    ClientBindAny(#[source] io::Error),
    #[error("Failed to connect to upstream: {source}, {addr}, {sock_addr}")]
    ConnectUpstream {
        #[source]
        source: io::Error,
        addr: InternetAddr,
        sock_addr: SocketAddr,
    },
    #[error("Failed to copy: {source}, {addr}, {sock_addr}")]
    Copy {
        #[source]
        source: CopyBiError,
        addr: InternetAddr,
        sock_addr: SocketAddr,
    },
}

fn error_kind_from_proxy_error(e: ProxyError) -> RouteErrorKind {
    match e {
        ProxyError::Resolve { .. }
        | ProxyError::ClientBindAny(_)
        | ProxyError::ConnectUpstream { .. }
        | ProxyError::Copy { .. } => RouteErrorKind::Io,
        ProxyError::Loopback { .. } => RouteErrorKind::Loopback,
    }
}

impl loading::Hook for UdpProxy {}

#[async_trait]
impl UdpServerHook for UdpProxy {
    async fn parse_upstream_addr(
        &self,
        buf: &mut io::Cursor<&[u8]>,
        downstream_writer: &UdpDownstreamWriter,
    ) -> Option<UpstreamAddr> {
        match steer(buf, downstream_writer, &self.header_crypto).await {
            Ok(res) => res,
            Err(_) => None,
        }
    }

    async fn handle_flow(
        &self,
        rx: mpsc::Receiver<Packet>,
        flow: Flow,
        downstream_writer: UdpDownstreamWriter,
    ) {
        let res = self.proxy(rx, flow, downstream_writer.clone()).await;
        self.handle_proxy_result(&downstream_writer, res).await;
    }
}
