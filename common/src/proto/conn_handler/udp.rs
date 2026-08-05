use std::{io, net::SocketAddr, sync::Arc};

use crate::{
    addr::InternetAddr,
    header::{
        codec::write_header,
        route::{RouteErrorKind, RouteResponse},
    },
    loading,
    proto::{
        conn::udp::{Flow, UpstreamAddr},
        context::UdpRuntime,
        relay::udp::{CopyBidirectional, DownstreamParts, UpstreamParts},
        route_header::udp::{decode_route_header, echo},
    },
    udp::{
        Packet,
        respond::respond_with_error,
        server::{UdpServer, UdpServerHandleConn},
    },
};
use async_speed_limit::Limiter;
use thiserror::Error;
use tokio::net::{ToSocketAddrs, UdpSocket};
use tracing::{instrument, trace, warn};
use udp_listener::{Conn, ConnWrite};

use super::ListenerBindError;

#[derive(Debug)]
pub struct UdpProxyConnHandler {
    header_crypto: tokio_chacha20::config::Config,
    payload_crypto: Option<tokio_chacha20::config::Config>,
    udp_context: UdpRuntime,
    allow_loopback: bool,
}
impl UdpProxyConnHandler {
    pub fn new(
        header_crypto: tokio_chacha20::config::Config,
        payload_crypto: Option<tokio_chacha20::config::Config>,
        udp_context: UdpRuntime,
        allow_loopback: bool,
    ) -> Self {
        Self {
            header_crypto,
            payload_crypto,
            udp_context,
            allow_loopback,
        }
    }

    pub async fn build(
        self,
        listen_addr: impl ToSocketAddrs,
    ) -> Result<UdpServer<Self>, ListenerBindError> {
        let listener = UdpSocket::bind(listen_addr)
            .await
            .map_err(ListenerBindError)?;
        let session_spawner = self.udp_context.session_spawner.clone();
        Ok(UdpServer::new(listener, self, session_spawner))
    }

    #[instrument(skip(self, conn))]
    async fn proxy(&self, mut conn: Conn<UdpSocket, Flow, Packet>) -> Result<(), UdpProxyError> {
        let flow = conn.conn_key().clone();
        if flow.upstream.is_none() {
            let pkt = conn.read_half().read_half().try_recv().unwrap();
            echo(pkt.slice(), conn.write(), &self.header_crypto).await;
            return Ok(());
        }
        let resolved_upstream = *flow
            .upstream
            .as_ref()
            .unwrap()
            .0
            .to_socket_addrs()
            .await
            .map_err(|e| UdpProxyError::Resolve {
                source: e,
                addr: flow.upstream.as_ref().unwrap().0.clone(),
            })?
            .first();
        if !self.allow_loopback && crate::addr::reaches_loopback(&resolved_upstream.ip()) {
            return Err(UdpProxyError::Loopback {
                addr: flow.upstream.as_ref().unwrap().0.clone(),
                sock_addr: resolved_upstream,
            });
        }
        let upstream = self
            .udp_context
            .connector
            .connect(resolved_upstream)
            .await
            .map_err(|e| UdpProxyError::ConnectUpstream {
                source: e,
                addr: flow.upstream.as_ref().unwrap().0.clone(),
                sock_addr: resolved_upstream,
            })?;
        let upstream = Arc::new(upstream);
        let header_crypto = self.header_crypto.clone();
        let response_header = move || {
            let mut wtr = Vec::new();
            let header = RouteResponse { result: Ok(()) };
            write_header(&mut wtr, &header, *header_crypto.key()).unwrap();
            wtr.into()
        };
        let header_crypto = self.header_crypto.clone();
        let payload_crypto = self.payload_crypto.clone();
        let session_table = self.udp_context.session_table.clone();
        let retention = self.udp_context.retention.clone();
        let upstream_local = upstream.local_addr().ok();
        let (dn_read, dn_write) = conn.split();
        let io_copy = CopyBidirectional {
            flow,
            upstream: UpstreamParts {
                read: upstream.clone(),
                write: upstream,
            },
            downstream: DownstreamParts {
                read: dn_read,
                write: dn_write.clone(),
            },
            speed_limiter: Limiter::new(f64::INFINITY),
            payload_crypto,
            response_header: Some(Box::new(response_header)),
            retention,
        };
        let res = io_copy
            .serve_as_proxy_server(session_table, upstream_local, "UDP")
            .await;
        if res.is_err() {
            let _ = respond_with_error(&dn_write, RouteErrorKind::Io, &header_crypto).await;
        }
        Ok(())
    }

    async fn handle_proxy_result(
        &self,
        dn_write: &ConnWrite<UdpSocket>,
        res: Result<(), UdpProxyError>,
    ) {
        match res {
            Ok(()) => (),
            Err(e) => {
                let peer_addr = dn_write.peer_addr();
                warn!(?e, ?peer_addr, "Proxy failed");
                let kind = error_kind_from_proxy_error(e);
                if let Err(e) = respond_with_error(dn_write, kind, &self.header_crypto).await {
                    trace!(?e, ?peer_addr, "Failed to respond with error");
                }
            }
        }
    }
}
#[derive(Debug, Error)]
pub enum UdpProxyError {
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
    #[error("Failed to connect to upstream: {source}, {addr}, {sock_addr}")]
    ConnectUpstream {
        #[source]
        source: io::Error,
        addr: InternetAddr,
        sock_addr: SocketAddr,
    },
}
fn error_kind_from_proxy_error(e: UdpProxyError) -> RouteErrorKind {
    match e {
        UdpProxyError::Resolve { .. } | UdpProxyError::ConnectUpstream { .. } => RouteErrorKind::Io,
        UdpProxyError::Loopback { .. } => RouteErrorKind::Loopback,
    }
}

impl loading::HandleConn for UdpProxyConnHandler {}
impl UdpServerHandleConn for UdpProxyConnHandler {
    fn parse_upstream_addr(&self, buf: &mut io::Cursor<&[u8]>) -> Option<Option<UpstreamAddr>> {
        let res = decode_route_header(buf, &self.header_crypto, &self.udp_context.time_validator);
        res.ok()
    }

    async fn handle_flow(&self, accepted: Conn<UdpSocket, Flow, Packet>) {
        let dn_write = accepted.write().clone();
        let res = self.proxy(accepted).await;
        self.handle_proxy_result(&dn_write, res).await;
    }
}
