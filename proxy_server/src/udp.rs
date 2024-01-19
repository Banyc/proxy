use std::{io, net::SocketAddr, sync::Arc};

use async_speed_limit::Limiter;
use common::{
    addr::{any_addr, InternetAddr},
    header::{
        codec::write_header,
        route::{RouteErrorKind, RouteResponse},
    },
    loading,
    udp::{
        context::UdpContext,
        io_copy::{CopyBidirectional, DownstreamParts, UpstreamParts},
        respond::respond_with_error,
        steer::steer,
        FlowOwnedGuard, Packet, UdpDownstreamWriter, UdpServer, UdpServerHook, UpstreamAddr,
    },
};
use serde::Deserialize;
use thiserror::Error;
use tokio::{
    net::{ToSocketAddrs, UdpSocket},
    sync::mpsc,
};
use tracing::{error, instrument, trace, warn};

use crate::ListenerBindError;

#[derive(Debug, Clone, PartialEq, Eq, Hash, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UdpProxyServerConfig {
    pub listen_addr: Arc<str>,
    pub header_key: tokio_chacha20::config::ConfigBuilder,
    pub payload_key: Option<tokio_chacha20::config::ConfigBuilder>,
}

#[derive(Debug, Clone)]
pub struct UdpProxyServerBuilder {
    pub config: UdpProxyServerConfig,
    pub udp_context: UdpContext,
}

impl loading::Builder for UdpProxyServerBuilder {
    type Hook = UdpProxy;
    type Server = UdpServer<Self::Hook>;
    type Err = UdpProxyServerBuildError;

    async fn build_server(self) -> Result<Self::Server, Self::Err> {
        let listen_addr = Arc::clone(&self.config.listen_addr);
        let udp_proxy = self.build_hook()?;
        let server = udp_proxy.build(listen_addr.as_ref()).await?;
        Ok(server)
    }

    fn build_hook(self) -> Result<Self::Hook, Self::Err> {
        let header_crypto = self
            .config
            .header_key
            .build()
            .map_err(UdpProxyBuildError::HeaderCrypto)?;
        let payload_crypto = match self.config.payload_key {
            Some(payload_crypto) => Some(
                payload_crypto
                    .build()
                    .map_err(UdpProxyBuildError::HeaderCrypto)?,
            ),
            None => None,
        };
        Ok(UdpProxy::new(
            header_crypto,
            payload_crypto,
            self.udp_context,
        ))
    }

    fn key(&self) -> &Arc<str> {
        &self.config.listen_addr
    }
}

#[derive(Debug, Error)]
pub enum UdpProxyBuildError {
    #[error("HeaderCrypto: {0}")]
    HeaderCrypto(#[source] tokio_chacha20::config::ConfigBuildError),
    #[error("PayloadCrypto: {0}")]
    PayloadCrypto(#[source] tokio_chacha20::config::ConfigBuildError),
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
    header_crypto: tokio_chacha20::config::Config,
    payload_crypto: Option<tokio_chacha20::config::Config>,
    udp_context: UdpContext,
}

impl UdpProxy {
    pub fn new(
        header_crypto: tokio_chacha20::config::Config,
        payload_crypto: Option<tokio_chacha20::config::Config>,
        udp_context: UdpContext,
    ) -> Self {
        Self {
            header_crypto,
            payload_crypto,
            udp_context,
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

    #[instrument(skip(self, rx, flow, downstream_writer))]
    async fn proxy(
        &self,
        rx: mpsc::Receiver<Packet>,
        flow: FlowOwnedGuard,
        downstream_writer: UdpDownstreamWriter,
    ) -> Result<(), ProxyError> {
        // Prevent connections to localhost
        let resolved_upstream =
            flow.flow()
                .upstream
                .0
                .to_socket_addr()
                .await
                .map_err(|e| ProxyError::Resolve {
                    source: e,
                    addr: flow.flow().upstream.0.clone(),
                })?;
        if resolved_upstream.ip().is_loopback() {
            return Err(ProxyError::Loopback {
                addr: flow.flow().upstream.0.clone(),
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
                addr: flow.flow().upstream.0.clone(),
                sock_addr: resolved_upstream,
            })?;
        let upstream = Arc::new(upstream);

        let response_header = {
            // Write header
            let mut wtr = Vec::new();
            let header = RouteResponse { result: Ok(()) };
            let mut crypto_cursor =
                tokio_chacha20::cursor::EncryptCursor::new(*self.header_crypto.key());
            write_header(&mut wtr, &header, &mut crypto_cursor).unwrap();
            wtr.into()
        };

        let header_crypto = self.header_crypto.clone();
        let payload_crypto = self.payload_crypto.clone();
        let session_table = self.udp_context.session_table.clone();
        let upstream_local = upstream.local_addr().ok();
        tokio::spawn(async move {
            let io_copy = CopyBidirectional {
                flow,
                upstream: UpstreamParts {
                    read: upstream.clone(),
                    write: upstream,
                },
                downstream: DownstreamParts {
                    rx,
                    write: downstream_writer.clone(),
                },
                speed_limiter: Limiter::new(f64::INFINITY),
                payload_crypto,
                response_header: Some(response_header),
            };
            let res = io_copy
                .serve_as_proxy_server(session_table, upstream_local, "UDP")
                .await;
            if res.is_err() {
                let _ = respond_with_error(&downstream_writer, RouteErrorKind::Io, &header_crypto)
                    .await;
            }
        });
        Ok(())
    }

    async fn handle_proxy_result(
        &self,
        downstream_writer: &UdpDownstreamWriter,
        res: Result<(), ProxyError>,
    ) {
        match res {
            Ok(()) => (),
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
}

fn error_kind_from_proxy_error(e: ProxyError) -> RouteErrorKind {
    match e {
        ProxyError::Resolve { .. }
        | ProxyError::ClientBindAny(_)
        | ProxyError::ConnectUpstream { .. } => RouteErrorKind::Io,
        ProxyError::Loopback { .. } => RouteErrorKind::Loopback,
    }
}

impl loading::Hook for UdpProxy {}

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
        flow: FlowOwnedGuard,
        downstream_writer: UdpDownstreamWriter,
    ) {
        let res = self.proxy(rx, flow, downstream_writer.clone()).await;
        self.handle_proxy_result(&downstream_writer, res).await;
    }
}
