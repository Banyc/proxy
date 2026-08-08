use std::{io, net::SocketAddr};

use crate::{
    error::AnyError,
    header::{
        codec::write_header,
        route::{RouteErrorKind, RouteResponse},
    },
    loading,
    proto::{
        conn::udp::{DownstreamAddr, Flow, UpstreamAddr},
        context::UdpRuntime,
        relay::udp::{CopyBidirectional, DownstreamParts, UdpRecv, UdpSend, UpstreamParts},
        route_header::udp::{decode_route_header, echo},
    },
    udp::{
        PACKET_BUFFER_LENGTH, Packet,
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
        let (dn_read, dn_write) = conn.split();
        self.proxy_parts(flow, dn_read, dn_write).await?;
        Ok(())
    }

    async fn proxy_parts<DownstreamRead, DownstreamWrite>(
        &self,
        flow: Flow,
        dn_read: DownstreamRead,
        dn_write: DownstreamWrite,
    ) -> Result<(), UdpProxyError>
    where
        DownstreamRead: UdpRecv + Send + 'static,
        DownstreamWrite: UdpSend + Send + 'static,
    {
        let upstream_route = &flow.upstream.as_ref().unwrap().0;
        if upstream_route.reverse_tunnel().is_none() {
            let resolved_upstream = *upstream_route
                .address
                .to_socket_addrs()
                .await
                .map_err(|e| UdpProxyError::Resolve {
                    source: e,
                    addr: upstream_route.clone(),
                })?
                .first();
            if !self.allow_loopback && crate::addr::reaches_loopback(&resolved_upstream.ip()) {
                return Err(UdpProxyError::Loopback {
                    addr: upstream_route.clone(),
                    sock_addr: resolved_upstream,
                });
            }
        }
        let upstream = self
            .udp_context
            .connector
            .connect_route(upstream_route, crate::STREAM_IO_TIMEOUT)
            .await
            .map_err(|e| UdpProxyError::ConnectUpstream {
                source: e,
                addr: upstream_route.clone(),
            })?;
        let header_crypto = self.header_crypto.clone();
        let response_header = move || {
            let mut wtr = Vec::new();
            let header = RouteResponse { result: Ok(()) };
            write_header(&mut wtr, &header, *header_crypto.key()).unwrap();
            wtr.into()
        };
        let payload_crypto = self.payload_crypto.clone();
        let session_table = self.udp_context.session_table.clone();
        let retention = self.udp_context.retention.clone();
        let upstream_local = upstream.local_addr();
        let (upstream_read, upstream_write) = upstream.into_split();
        let io_copy = CopyBidirectional {
            flow,
            upstream: UpstreamParts {
                read: upstream_read,
                write: upstream_write,
            },
            downstream: DownstreamParts {
                read: dn_read,
                write: dn_write,
            },
            speed_limiter: Limiter::new(f64::INFINITY),
            payload_crypto,
            response_header: Some(Box::new(response_header)),
            retention,
        };
        io_copy
            .serve_as_proxy_server(session_table, upstream_local, "UDP")
            .await
            .map_err(UdpProxyError::Copy)?;
        Ok(())
    }

    pub async fn handle_tunnel_flow<Read, Write>(
        &self,
        read: Read,
        mut write: Write,
        downstream: SocketAddr,
    ) -> Result<(), UdpProxyError>
    where
        Read: UdpRecv + Send + 'static,
        Write: UdpSend + Send + 'static,
    {
        let mut read = RouteDecodingRecv::new(
            read,
            self.header_crypto.clone(),
            self.udp_context.time_validator.clone(),
        );
        let upstream = match read.prime().await {
            Ok(upstream) => upstream,
            // The client closed the flow before sending any datagram
            // (e.g. an abandoned probe or a torn-down flow). This is a
            // clean close, not an error.
            Err(error) if is_udp_eof(&error) => return Ok(()),
            Err(error) => return Err(UdpProxyError::Tunnel(error)),
        };
        if upstream.is_none() {
            let mut payload = [0; PACKET_BUFFER_LENGTH];
            loop {
                let n = match read.trait_recv(&mut payload).await {
                    Ok(n) => n,
                    // The probing client closed its flow after the echo;
                    // end this flow cleanly instead of logging a warning.
                    Err(error) if is_udp_eof(&error) => return Ok(()),
                    Err(error) => return Err(UdpProxyError::Tunnel(error)),
                };
                let mut response = Vec::new();
                write_header(
                    &mut response,
                    &RouteResponse { result: Ok(()) },
                    *self.header_crypto.key(),
                )
                .map_err(|error| UdpProxyError::Tunnel(Box::new(error)))?;
                response.extend_from_slice(&payload[..n]);
                write
                    .trait_send(&response)
                    .await
                    .map_err(UdpProxyError::Tunnel)?;
            }
        }
        let flow = Flow {
            upstream,
            downstream: DownstreamAddr(downstream),
        };
        self.proxy_parts(flow, read, write).await
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
        addr: crate::proto::addr::RouteAddr,
    },
    #[error("Refused to connect to a loopback address: {addr}, {sock_addr}")]
    Loopback {
        addr: crate::proto::addr::RouteAddr,
        sock_addr: SocketAddr,
    },
    #[error("Failed to connect to upstream: {source}, {addr}")]
    ConnectUpstream {
        #[source]
        source: io::Error,
        addr: crate::proto::addr::RouteAddr,
    },
    #[error("reverse-tunnel UDP flow: {0}")]
    Tunnel(#[source] AnyError),
    #[error("UDP relay: {0}")]
    Copy(#[source] crate::proto::relay::udp::CopyBiError),
}
fn error_kind_from_proxy_error(e: UdpProxyError) -> RouteErrorKind {
    match e {
        UdpProxyError::Resolve { .. } | UdpProxyError::ConnectUpstream { .. } => RouteErrorKind::Io,
        UdpProxyError::Loopback { .. } => RouteErrorKind::Loopback,
        UdpProxyError::Tunnel(_) | UdpProxyError::Copy(_) => RouteErrorKind::Io,
    }
}

struct RouteDecodingRecv<Read> {
    inner: Read,
    header_crypto: tokio_chacha20::config::Config,
    time_validator: std::sync::Arc<ae::anti_replay::TimeValidator>,
    expected: Option<Option<UpstreamAddr>>,
    first_payload: Option<Vec<u8>>,
    packet: Vec<u8>,
}
impl<Read> RouteDecodingRecv<Read>
where
    Read: UdpRecv,
{
    fn new(
        inner: Read,
        header_crypto: tokio_chacha20::config::Config,
        time_validator: std::sync::Arc<ae::anti_replay::TimeValidator>,
    ) -> Self {
        Self {
            inner,
            header_crypto,
            time_validator,
            expected: None,
            first_payload: None,
            packet: vec![0; PACKET_BUFFER_LENGTH],
        }
    }
    async fn prime(&mut self) -> Result<Option<UpstreamAddr>, AnyError> {
        let (upstream, payload) = self.recv_decoded().await?;
        self.expected = Some(upstream.clone());
        self.first_payload = Some(payload);
        Ok(upstream)
    }
    async fn recv_decoded(&mut self) -> Result<(Option<UpstreamAddr>, Vec<u8>), AnyError> {
        let n = self.inner.trait_recv(&mut self.packet).await?;
        let mut cursor = io::Cursor::new(&self.packet[..n]);
        let upstream = decode_route_header(&mut cursor, &self.header_crypto, &self.time_validator)?;
        let payload = self.packet[usize::try_from(cursor.position()).unwrap()..n].to_vec();
        Ok((upstream, payload))
    }
}
impl<Read> UdpRecv for RouteDecodingRecv<Read>
where
    Read: UdpRecv + Send,
{
    async fn trait_recv(&mut self, buf: &mut [u8]) -> Result<usize, AnyError> {
        let payload = if let Some(payload) = self.first_payload.take() {
            payload
        } else {
            let (upstream, payload) = self.recv_decoded().await?;
            if self.expected.as_ref() != Some(&upstream) {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "UDP mux flow changed its upstream route",
                )
                .into());
            }
            payload
        };
        let copied = payload.len().min(buf.len());
        buf[..copied].copy_from_slice(&payload[..copied]);
        Ok(copied)
    }
}

/// `true` if the error is a clean EOF from the mux substream, i.e. the
/// client closed the flow rather than failing mid-datagram.
fn is_udp_eof(error: &AnyError) -> bool {
    error
        .downcast_ref::<io::Error>()
        .is_some_and(|error| error.kind() == io::ErrorKind::UnexpectedEof)
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
