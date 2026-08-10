use std::{io, net::SocketAddr, time::Duration};

use crate::{
    error::AnyError,
    header::{
        codec::write_header,
        route::{RouteErrorKind, RouteResponse},
    },
    loading,
    proto::{
        conn::udp::{DownstreamAddr, Flow, FlowKey, UdpFlowId, UpstreamAddr},
        context::UdpRuntime,
        relay::udp::{CopyBidirectional, DownstreamParts, UdpRecv, UdpSend, UpstreamParts},
        route_header::udp::{UdpRequestRoute, decode_request_route, echo},
    },
    udp::{
        PACKET_BUFFER_LENGTH, Packet, UDP_FLOW_TIMEOUT,
        respond::respond_with_error,
        server::{UdpPacketRoute, UdpServer, UdpServerHandleConn},
    },
};
use async_speed_limit::Limiter;
use metrics::counter;
use thiserror::Error;
use tokio::net::{ToSocketAddrs, UdpSocket};
use tracing::{debug, instrument, trace, warn};
use udp_listener::{Conn, ConnRead, ConnWrite};

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
    async fn proxy(&self, mut conn: Conn<UdpSocket, FlowKey, Packet>) -> Result<(), UdpProxyError> {
        let downstream = match conn.conn_key() {
            FlowKey::Identified { downstream, .. } => *downstream,
            FlowKey::Routed(_) => return Err(UdpProxyError::InvalidFlowKey),
        };
        let first = conn
            .read_half()
            .read_half()
            .try_recv()
            .map_err(|error| UdpProxyError::InitialPacket(io::Error::other(error)))?;
        let Some(upstream) = first.routed_upstream().cloned() else {
            return Err(UdpProxyError::RouteUnavailable);
        };
        let flow = Flow {
            upstream: upstream.clone(),
            downstream,
        };
        if upstream.is_none() {
            echo(first.slice(), conn.write(), &self.header_crypto).await;
            return Ok(());
        }
        let (dn_read, dn_write) = conn.split();
        let dn_read = RouteBoundRecv::new(dn_read, upstream, first);
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
        write: Write,
        downstream: SocketAddr,
    ) -> Result<(), UdpProxyError>
    where
        Read: UdpRecv + Send + 'static,
        Write: UdpSend + Send + 'static,
    {
        self.handle_tunnel_flow_with_timeouts(
            read,
            write,
            downstream,
            crate::STREAM_IO_TIMEOUT,
            UDP_FLOW_TIMEOUT,
        )
        .await
    }

    /// [`Self::handle_tunnel_flow`] with injectable timeouts: `prime_timeout`
    /// bounds the establishing read, `echo_timeout` bounds each read and
    /// each response write of an echo flow, so a stalled peer releases its
    /// bounded session slot. The public entry point uses
    /// [`crate::STREAM_IO_TIMEOUT`] / [`UDP_FLOW_TIMEOUT`]; tests pass short
    /// durations.
    async fn handle_tunnel_flow_with_timeouts<Read, Write>(
        &self,
        read: Read,
        write: Write,
        downstream: SocketAddr,
        prime_timeout: Duration,
        echo_timeout: Duration,
    ) -> Result<(), UdpProxyError>
    where
        Read: UdpRecv + Send + 'static,
        Write: UdpSend + Send + 'static,
    {
        let tunnel = match self.establish_tunnel(read, prime_timeout).await {
            Ok(tunnel) => tunnel,
            // The client closed the flow before sending any datagram
            // (e.g. an abandoned probe or a torn-down flow). This is a
            // clean close, not an error.
            Err(UdpProxyError::Tunnel(error)) if is_udp_eof(&error) => return Ok(()),
            Err(error) => return Err(error),
        };
        match tunnel {
            EstablishedTunnel::Echo(flow) => match flow.run(write, echo_timeout).await {
                // An echo flow that went idle has no peer left to answer; the
                // safety timeout is its designed end. Count the expiry and log
                // it at debug instead of surfacing a warning.
                Err(UdpProxyError::Tunnel(error)) if is_echo_flow_idle(&error) => {
                    counter!("udp.echo_flow_idle_timeouts").increment(1);
                    debug!(?error, %downstream, "UDP echo flow idled out");
                    Ok(())
                }
                result => result,
            },
            EstablishedTunnel::Relay(flow) => {
                let (read, upstream) = flow.into_parts();
                let flow = Flow {
                    upstream: Some(upstream),
                    downstream: DownstreamAddr(downstream),
                };
                self.proxy_parts(flow, read, write).await
            }
        }
    }

    /// Establish a tunnel flow from its first routed datagram.
    ///
    /// Reads one datagram under `prime_timeout` and decodes its route, then
    /// returns the typed flow: an [`EchoFlow`] when the client requested no
    /// upstream, a [`RelayFlow`] bound to the requested upstream otherwise.
    /// After this returns, every flow state is established and non-optional.
    async fn establish_tunnel<Read>(
        &self,
        mut read: Read,
        prime_timeout: Duration,
    ) -> Result<EstablishedTunnel<Read>, UdpProxyError>
    where
        Read: UdpRecv + Send,
    {
        let mut packet = vec![0; PACKET_BUFFER_LENGTH];
        let n = match tokio::time::timeout(prime_timeout, read.trait_recv(&mut packet)).await {
            Ok(Ok(n)) => n,
            Ok(Err(error)) => return Err(UdpProxyError::Tunnel(error)),
            // A client that opened the flow but never sent the first routed
            // datagram must not hold a bounded session slot forever.
            Err(_) => {
                return Err(flow_timed_out(
                    "timed out waiting for the first routed datagram",
                ));
            }
        };
        let mut cursor = io::Cursor::new(&packet[..n]);
        let route = decode_request_route(
            &mut cursor,
            &self.header_crypto,
            &self.udp_context.time_validator,
        )
        .map_err(|error| UdpProxyError::Tunnel(Box::new(error)))?;
        let UdpRequestRoute::Routed { flow_id, upstream } = route else {
            return Err(UdpProxyError::Tunnel(
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    "UDP mux flow used compact form before establishing its route",
                )
                .into(),
            ));
        };
        let payload = packet[usize::try_from(cursor.position()).unwrap()..n].to_vec();
        let decoder = FlowRecv::new(
            read,
            self.header_crypto.clone(),
            self.udp_context.time_validator.clone(),
            flow_id,
            upstream.clone(),
        );
        Ok(match upstream {
            None => EstablishedTunnel::Echo(EchoFlow {
                decoder,
                first: payload,
            }),
            Some(upstream) => EstablishedTunnel::Relay(RelayFlow {
                decoder,
                first: payload,
                upstream,
            }),
        })
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
                if matches!(e, UdpProxyError::RouteUnavailable) {
                    trace!(?e, ?peer_addr, "Dropping unrouted compact flow");
                    return;
                }
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
    #[error("UDP flow key is not a routed flow")]
    InvalidFlowKey,
    #[error("Failed to receive the initial packet: {0}")]
    InitialPacket(#[source] io::Error),
    #[error("UDP flow has no authenticated upstream route")]
    RouteUnavailable,
}
fn error_kind_from_proxy_error(e: UdpProxyError) -> RouteErrorKind {
    match e {
        UdpProxyError::Resolve { .. } | UdpProxyError::ConnectUpstream { .. } => RouteErrorKind::Io,
        UdpProxyError::Loopback { .. } => RouteErrorKind::Loopback,
        UdpProxyError::Tunnel(_)
        | UdpProxyError::Copy(_)
        | UdpProxyError::InvalidFlowKey
        | UdpProxyError::InitialPacket(_)
        | UdpProxyError::RouteUnavailable => RouteErrorKind::Io,
    }
}

struct RouteBoundRecv {
    inner: ConnRead<Packet>,
    expected_upstream: Option<UpstreamAddr>,
    first: Option<Packet>,
}
impl RouteBoundRecv {
    fn new(
        inner: ConnRead<Packet>,
        expected_upstream: Option<UpstreamAddr>,
        first: Packet,
    ) -> Self {
        Self {
            inner,
            expected_upstream,
            first: Some(first),
        }
    }
}
impl UdpRecv for RouteBoundRecv {
    async fn trait_recv(&mut self, buf: &mut [u8]) -> Result<usize, AnyError> {
        let packet =
            match self.first.take() {
                Some(packet) => packet,
                None => self.inner.read_half().recv().await.ok_or_else(|| {
                    io::Error::new(io::ErrorKind::UnexpectedEof, "UDP flow closed")
                })?,
            };
        if let Some(upstream) = packet.routed_upstream()
            && upstream != &self.expected_upstream
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "UDP flow changed its upstream route",
            )
            .into());
        }
        let copied = packet.slice().len().min(buf.len());
        buf[..copied].copy_from_slice(&packet.slice()[..copied]);
        Ok(copied)
    }
}

/// A UDP tunnel flow established from the first routed datagram: either an
/// echo flow (no upstream) or a relay flow bound to a fixed upstream. Each
/// variant holds only established, non-optional state.
enum EstablishedTunnel<R> {
    /// The client requested no upstream: every datagram is answered with a
    /// `RouteResponse` header plus the echoed payload.
    Echo(EchoFlow<R>),
    /// A routed flow: the establishing datagram and every subsequent one is
    /// relayed to the bound upstream.
    Relay(RelayFlow<R>),
}

/// An echo tunnel flow, running until the peer closes it or goes idle.
struct EchoFlow<R> {
    decoder: FlowRecv<R>,
    /// The establishing datagram's payload, echoed first.
    first: Vec<u8>,
}

impl<R> EchoFlow<R>
where
    R: UdpRecv + Send + 'static,
{
    /// Echo every datagram on the flow, bounding each read and each response
    /// write by `timeout`. Always shuts down the response writer before
    /// returning, so the client's read half sees a clean EOF and the flow's
    /// session slot is released promptly instead of idling out.
    async fn run<W>(self, mut write: W, timeout: Duration) -> Result<(), UdpProxyError>
    where
        W: UdpSend + Send + 'static,
    {
        let EchoFlow { mut decoder, first } = self;
        let result = async {
            Self::respond(&decoder.header_crypto, &mut write, &first, timeout).await?;
            let mut payload = [0; PACKET_BUFFER_LENGTH];
            loop {
                let n = match tokio::time::timeout(timeout, decoder.trait_recv(&mut payload)).await
                {
                    Ok(Ok(n)) => n,
                    // The probing client closed its flow after the echo; end this
                    // flow cleanly instead of logging a warning.
                    Ok(Err(error)) if is_udp_eof(&error) => return Ok(()),
                    Ok(Err(error)) => return Err(UdpProxyError::Tunnel(error)),
                    // An idle echo flow holds its session slot; time it out so
                    // the slot is released.
                    Err(_) => return Err(flow_timed_out(ECHO_FLOW_IDLE)),
                };
                Self::respond(&decoder.header_crypto, &mut write, &payload[..n], timeout).await?;
            }
        }
        .await;
        // Explicit epilog: close the response writer so the peer's read half
        // sees a clean EOF. The peer may already have closed its read half;
        // that is the same clean end, not a failure of the echo flow.
        match write.trait_shutdown().await {
            Ok(()) => {}
            Err(error) => trace!(?error, "Echo flow response writer shutdown failed"),
        }
        result
    }

    /// Write one echoed datagram: a `RouteResponse` header followed by the
    /// payload, bounded by `timeout`.
    async fn respond<W>(
        header_crypto: &tokio_chacha20::config::Config,
        write: &mut W,
        payload: &[u8],
        timeout: Duration,
    ) -> Result<(), UdpProxyError>
    where
        W: UdpSend,
    {
        let mut response = Vec::new();
        write_header(
            &mut response,
            &RouteResponse { result: Ok(()) },
            *header_crypto.key(),
        )
        .map_err(|error| UdpProxyError::Tunnel(Box::new(error)))?;
        response.extend_from_slice(payload);
        match tokio::time::timeout(timeout, write.trait_send(&response)).await {
            Ok(Ok(_)) => Ok(()),
            Ok(Err(error)) => Err(UdpProxyError::Tunnel(error)),
            // A peer that stops reading its echo responses lets the send
            // block forever; time the write out so the slot is released.
            Err(_) => Err(flow_timed_out("UDP echo flow stalled on response write")),
        }
    }
}

/// A relay tunnel flow bound to a fixed upstream route.
struct RelayFlow<R> {
    decoder: FlowRecv<R>,
    /// The establishing datagram's payload, relayed first.
    first: Vec<u8>,
    upstream: UpstreamAddr,
}

impl<R> RelayFlow<R> {
    /// Split into the relay's downstream read (the establishing datagram
    /// first, then decoded datagrams) and the upstream route.
    fn into_parts(self) -> (RelayRead<R>, UpstreamAddr) {
        (
            RelayRead {
                decoder: self.decoder,
                pending: Some(self.first),
            },
            self.upstream,
        )
    }
}

/// A relay flow's downstream read: yields the establishing datagram once,
/// then falls through to decoding datagrams bound to the flow.
struct RelayRead<R> {
    decoder: FlowRecv<R>,
    /// The establishing datagram's payload, delivered once.
    pending: Option<Vec<u8>>,
}
impl<R> UdpRecv for RelayRead<R>
where
    R: UdpRecv + Send,
{
    async fn trait_recv(&mut self, buf: &mut [u8]) -> Result<usize, AnyError> {
        if let Some(payload) = self.pending.take() {
            let copied = payload.len().min(buf.len());
            buf[..copied].copy_from_slice(&payload[..copied]);
            return Ok(copied);
        }
        self.decoder.trait_recv(buf).await
    }
}

/// A read half that decodes UDP route headers and validates every datagram
/// against the flow it was established with. `flow_id` is fixed and
/// `upstream` is the route the flow was bound to (`None` for echo flows) —
/// there is no not-yet-established state.
struct FlowRecv<Read> {
    inner: Read,
    header_crypto: tokio_chacha20::config::Config,
    time_validator: std::sync::Arc<ae::anti_replay::TimeValidator>,
    flow_id: UdpFlowId,
    upstream: Option<UpstreamAddr>,
    packet: Vec<u8>,
}
impl<Read> FlowRecv<Read>
where
    Read: UdpRecv,
{
    fn new(
        inner: Read,
        header_crypto: tokio_chacha20::config::Config,
        time_validator: std::sync::Arc<ae::anti_replay::TimeValidator>,
        flow_id: UdpFlowId,
        upstream: Option<UpstreamAddr>,
    ) -> Self {
        Self {
            inner,
            header_crypto,
            time_validator,
            flow_id,
            upstream,
            packet: vec![0; PACKET_BUFFER_LENGTH],
        }
    }
    async fn recv_decoded(&mut self) -> Result<Vec<u8>, AnyError> {
        let n = self.inner.trait_recv(&mut self.packet).await?;
        let mut cursor = io::Cursor::new(&self.packet[..n]);
        let route = decode_request_route(&mut cursor, &self.header_crypto, &self.time_validator)?;
        let mismatched = match route {
            UdpRequestRoute::Compact { flow_id } => flow_id != self.flow_id,
            UdpRequestRoute::Routed { flow_id, upstream } => {
                flow_id != self.flow_id || upstream != self.upstream
            }
        };
        if mismatched {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "UDP mux flow changed its flow ID or upstream route",
            )
            .into());
        }
        Ok(self.packet[usize::try_from(cursor.position()).unwrap()..n].to_vec())
    }
}
impl<Read> UdpRecv for FlowRecv<Read>
where
    Read: UdpRecv + Send,
{
    async fn trait_recv(&mut self, buf: &mut [u8]) -> Result<usize, AnyError> {
        let payload = self.recv_decoded().await?;
        let copied = payload.len().min(buf.len());
        buf[..copied].copy_from_slice(&payload[..copied]);
        Ok(copied)
    }
}

/// `true` if the error is a clean EOF at a datagram boundary.
///
/// `UdpMuxReader` reports `UnexpectedEof` only when the peer closed its
/// write half between datagrams; EOF that cuts a length prefix or payload
/// short is reported as `InvalidData` and must propagate as corruption.
fn is_udp_eof(error: &AnyError) -> bool {
    error
        .downcast_ref::<io::Error>()
        .is_some_and(|error| error.kind() == io::ErrorKind::UnexpectedEof)
}

/// The idle-expiry message for an echo flow that never received its epilog:
/// the peer stalled instead of closing the flow, so the safety timeout fired.
const ECHO_FLOW_IDLE: &str = "UDP echo flow idle";

/// `true` if the error is the echo-flow idle expiry: the peer stalled
/// instead of completing the flow, so it ended by its safety timeout rather
/// than by a clean close.
fn is_echo_flow_idle(error: &AnyError) -> bool {
    error.downcast_ref::<io::Error>().is_some_and(|error| {
        error.kind() == io::ErrorKind::TimedOut && error.to_string() == ECHO_FLOW_IDLE
    })
}

/// A `TimedOut` tunnel-flow error: the peer stalled instead of completing
/// (or continuing) the flow, so the bounded session slot must be released.
fn flow_timed_out(what: &'static str) -> UdpProxyError {
    UdpProxyError::Tunnel(io::Error::new(io::ErrorKind::TimedOut, what).into())
}

impl loading::HandleConn for UdpProxyConnHandler {}
impl UdpServerHandleConn for UdpProxyConnHandler {
    fn parse_packet_route(&self, buf: &mut io::Cursor<&[u8]>) -> Option<UdpPacketRoute> {
        let route =
            decode_request_route(buf, &self.header_crypto, &self.udp_context.time_validator)
                .ok()?;
        Some(match route {
            UdpRequestRoute::Routed { flow_id, upstream } => UdpPacketRoute::Routed {
                flow_id: Some(flow_id),
                upstream,
            },
            UdpRequestRoute::Compact { flow_id } => UdpPacketRoute::Compact { flow_id },
        })
    }

    async fn handle_flow(&self, accepted: Conn<UdpSocket, FlowKey, Packet>) {
        let dn_write = accepted.write().clone();
        let res = self.proxy(accepted).await;
        self.handle_proxy_result(&dn_write, res).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        anti_replay::{VALIDATOR_TIME_FRAME, VALIDATOR_UDP_HDR_TTL},
        connect::{ConnectorConfig, ConnectorConfigHandle},
        header::route::RouteRequest,
        proto::{conn::udp::UDP_FLOW_ID_LEN, connect::udp::UdpConnector, context::UdpRuntime},
    };
    use ae::anti_replay::TimeValidator;
    use std::sync::Arc;

    fn crypto() -> tokio_chacha20::config::Config {
        tokio_chacha20::config::Config::new([7; tokio_chacha20::KEY_BYTES].into())
    }

    fn handler() -> UdpProxyConnHandler {
        let (session_spawner, _session_rx) = crate::session::SessionSpawner::channel();
        let (_retention_actor, retention) = crate::retention::RetentionActor::new();
        let udp_context = UdpRuntime {
            session_table: None,
            connector: Arc::new(UdpConnector::new(ConnectorConfigHandle::new(
                ConnectorConfig::default(),
            ))),
            time_validator: Arc::new(TimeValidator::new(
                VALIDATOR_TIME_FRAME + VALIDATOR_UDP_HDR_TTL,
            )),
            session_spawner,
            retention,
        };
        UdpProxyConnHandler::new(crypto(), None, udp_context, true)
    }

    /// A `UdpRecv` that returns its queued packet once and then stalls
    /// forever — modelling a client that sends the flow-kind byte (and
    /// maybe one datagram) and never continues.
    struct StallingRecv {
        packet: Option<Vec<u8>>,
    }
    impl UdpRecv for StallingRecv {
        async fn trait_recv(&mut self, buf: &mut [u8]) -> Result<usize, AnyError> {
            match self.packet.take() {
                Some(packet) => {
                    let n = packet.len().min(buf.len());
                    buf[..n].copy_from_slice(&packet[..n]);
                    Ok(n)
                }
                None => std::future::pending().await,
            }
        }
    }

    #[derive(Debug)]
    struct NoopSend;
    impl UdpSend for NoopSend {
        async fn trait_send(&mut self, _buf: &[u8]) -> Result<usize, AnyError> {
            Ok(0)
        }
    }

    /// A `UdpSend` that never resolves — modelling a peer that stops
    /// reading its echo responses, so the response write blocks forever.
    struct StallingSend;
    impl UdpSend for StallingSend {
        async fn trait_send(&mut self, _buf: &[u8]) -> Result<usize, AnyError> {
            std::future::pending().await
        }
    }

    /// A valid routed request with no upstream, i.e. an echo flow.
    fn routed_echo_packet() -> Vec<u8> {
        let mut packet = Vec::new();
        UdpFlowId::from_bytes([9; UDP_FLOW_ID_LEN]).write_routed(&mut packet);
        write_header(
            &mut packet,
            &RouteRequest::<crate::proto::addr::RouteAddr> { upstream: None },
            *crypto().key(),
        )
        .unwrap();
        packet
    }

    /// A valid routed request with an upstream, i.e. a relay flow.
    fn routed_relay_packet() -> Vec<u8> {
        let mut packet = Vec::new();
        UdpFlowId::from_bytes([8; UDP_FLOW_ID_LEN]).write_routed(&mut packet);
        write_header(
            &mut packet,
            &RouteRequest::<crate::proto::addr::RouteAddr> {
                upstream: Some(crate::proto::addr::RouteAddr::udp(
                    "127.0.0.1:9".parse().unwrap(),
                )),
            },
            *crypto().key(),
        )
        .unwrap();
        packet
    }

    /// The establishing datagram's route decides the flow kind: no upstream
    /// becomes an echo flow, a routed upstream becomes a relay flow bound to
    /// that upstream.
    #[tokio::test]
    async fn establishment_classifies_echo_and_relay_flows() {
        let handler = handler();
        let echo = handler
            .establish_tunnel(
                StallingRecv {
                    packet: Some(routed_echo_packet()),
                },
                Duration::from_millis(50),
            )
            .await
            .unwrap();
        assert!(matches!(echo, EstablishedTunnel::Echo(_)));

        let relay = handler
            .establish_tunnel(
                StallingRecv {
                    packet: Some(routed_relay_packet()),
                },
                Duration::from_millis(50),
            )
            .await
            .unwrap();
        match relay {
            EstablishedTunnel::Relay(flow) => {
                assert_eq!(
                    flow.upstream.0,
                    crate::proto::addr::RouteAddr::udp("127.0.0.1:9".parse().unwrap())
                );
            }
            _ => panic!("expected a relay flow"),
        }
    }

    fn assert_timed_out(result: Result<(), UdpProxyError>, expected: &str) {
        match result {
            Err(UdpProxyError::Tunnel(error)) => {
                let error = error.downcast_ref::<io::Error>().expect("io error");
                assert_eq!(error.kind(), io::ErrorKind::TimedOut);
                assert_eq!(error.to_string(), expected);
            }
            other => panic!("expected a timed-out tunnel error, got {other:?}"),
        }
    }

    /// A client that sends the UDP flow-kind byte and then stalls must not
    /// hold its bounded session slot forever: the first routed datagram is
    /// bounded by [`crate::STREAM_IO_TIMEOUT`].
    #[tokio::test]
    async fn a_stalled_initial_datagram_times_out_the_flow() {
        let result = handler()
            .handle_tunnel_flow_with_timeouts(
                StallingRecv { packet: None },
                NoopSend,
                "127.0.0.1:1".parse().unwrap(),
                Duration::from_millis(50),
                Duration::from_millis(50),
            )
            .await;
        assert_timed_out(result, "timed out waiting for the first routed datagram");
    }

    /// An echo flow that goes idle still releases its session slot: each
    /// echo read is bounded by [`crate::udp::UDP_FLOW_TIMEOUT`], and the
    /// expiry is a quiet clean end (metric + debug) rather than a warning.
    #[tokio::test]
    async fn an_idle_echo_flow_ends_quietly_and_releases_its_slot() {
        let result = handler()
            .handle_tunnel_flow_with_timeouts(
                StallingRecv {
                    packet: Some(routed_echo_packet()),
                },
                NoopSend,
                "127.0.0.1:1".parse().unwrap(),
                Duration::from_millis(50),
                Duration::from_millis(50),
            )
            .await;
        assert!(
            result.is_ok(),
            "an idle echo flow must end as a clean close: {result:?}"
        );
    }

    /// The echo loop itself still reports the idle expiry as a timed-out
    /// tunnel error; [`UdpProxyConnHandler::handle_tunnel_flow_with_timeouts`]
    /// is what turns it into a quiet clean end.
    #[tokio::test]
    async fn echo_flow_run_reports_idle_expiry_as_a_timeout() {
        let handler = handler();
        let flow = EchoFlow {
            decoder: FlowRecv::new(
                StallingRecv {
                    packet: Some(routed_echo_packet()),
                },
                handler.header_crypto.clone(),
                handler.udp_context.time_validator.clone(),
                UdpFlowId::from_bytes([9; UDP_FLOW_ID_LEN]),
                None,
            ),
            first: Vec::new(),
        };
        let result = flow.run(NoopSend, Duration::from_millis(50)).await;
        assert_timed_out(result, ECHO_FLOW_IDLE);
    }

    /// A peer that keeps sending echo requests but never reads the
    /// responses must not hold its session slot forever: the response
    /// write is bounded by [`crate::udp::UDP_FLOW_TIMEOUT`].
    #[tokio::test]
    async fn a_stalled_echo_writer_times_out_the_flow() {
        let result = handler()
            .handle_tunnel_flow_with_timeouts(
                StallingRecv {
                    packet: Some(routed_echo_packet()),
                },
                StallingSend,
                "127.0.0.1:1".parse().unwrap(),
                Duration::from_millis(50),
                Duration::from_millis(50),
            )
            .await;
        assert_timed_out(result, "UDP echo flow stalled on response write");
    }
}
