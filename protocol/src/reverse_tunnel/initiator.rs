//! The initiator side of a reverse tunnel: connects to the responder,
//! registers its name, and dispatches accepted streams/UDP flows to the
//! stream/UDP proxy handlers.

use std::{
    io,
    net::SocketAddr,
    sync::Arc,
    time::{Duration, Instant},
};

use ae::anti_replay::ValidatorRef;
use common::{
    error::AnyResult,
    header::codec::{timed_read_header_async, timed_write_header_async},
    loading,
    proto::{
        addr::{ReverseTunnelTransport, RouteAddr, validate_reverse_tunnel_name},
        conn_handler::{stream::StreamProxyConnHandler, udp::UdpProxyConnHandler},
        context::{Runtime, StreamRuntime},
    },
    session::log_rejection,
    stream::{IoConnection, StreamServerHandleConn},
};
use metrics::counter;
use mux::{LaneClass, MuxError, spawn_mux_no_reconnection};
use tokio::task::JoinSet;
use tracing::warn;

use crate::stream::streams::mux::{
    AddressedMuxStream, MuxProxyConnHandler, SocketAddrPair, client_mux_config, dispatch_mux_flow,
};

use super::{
    loading::{BuildError, ReverseTunnelInitiatorConfig},
    wire::{
        REGISTER_VERSION, RegisterRequest, RegisterResponse, ReverseTunnelSessionError, mux_result,
    },
};

const INITIAL_RECONNECT_DELAY: Duration = Duration::from_millis(250);
const MAX_RECONNECT_DELAY: Duration = Duration::from_secs(30);
const STABLE_SESSION: Duration = Duration::from_secs(30);

#[derive(Debug)]
pub struct ReverseTunnelInitiatorHandler {
    pub(crate) name: Arc<str>,
    pub(crate) responder_addr: RouteAddr,
    pub(crate) transport: ReverseTunnelTransport,
    pub(crate) registration_crypto: tokio_chacha20::config::Config,
    pub(crate) stream_proxy: Arc<StreamProxyConnHandler>,
    pub(crate) udp_proxy: Arc<UdpProxyConnHandler>,
    pub(crate) stream_runtime: StreamRuntime,
}
impl loading::HandleConn for ReverseTunnelInitiatorHandler {}

#[derive(Debug)]
pub struct ReverseTunnelInitiator {
    pub(crate) handler: ReverseTunnelInitiatorHandler,
}
impl loading::Serve for ReverseTunnelInitiator {
    type ConnHandler = ReverseTunnelInitiatorHandler;
    async fn serve(
        self,
        mut replacement_rx: loading::ReplaceConnHandlerRx<Self::ConnHandler>,
    ) -> AnyResult {
        let mut handler = Arc::new(self.handler);
        let mut reconnect_delay = INITIAL_RECONNECT_DELAY;
        loop {
            let started = Instant::now();
            tokio::select! {
                result = run_initiator_session(Arc::clone(&handler)) => {
                    warn!(?result, name = %handler.name, responder = %handler.responder_addr, "Reverse tunnel session ended");
                    counter!("revtun.session.disconnected").increment(1);
                    if started.elapsed() >= STABLE_SESSION {
                        reconnect_delay = INITIAL_RECONNECT_DELAY;
                    }
                }
                replacement = replacement_rx.recv() => {
                    match replacement {
                        Ok(Some(new_handler)) => {
                            handler = new_handler;
                            reconnect_delay = INITIAL_RECONNECT_DELAY;
                            continue;
                        }
                        Ok(None) => continue,
                        Err(()) => return Ok(()),
                    }
                }
            }
            tokio::select! {
                () = tokio::time::sleep(reconnect_delay) => {
                    reconnect_delay = reconnect_delay.saturating_mul(2).min(MAX_RECONNECT_DELAY);
                }
                replacement = replacement_rx.recv() => {
                    match replacement {
                        Ok(Some(new_handler)) => {
                            handler = new_handler;
                            reconnect_delay = INITIAL_RECONNECT_DELAY;
                        }
                        Ok(None) => {}
                        Err(()) => return Ok(()),
                    }
                }
            }
        }
    }
}

async fn run_initiator_session(
    handler: Arc<ReverseTunnelInitiatorHandler>,
) -> Result<(), ReverseTunnelSessionError> {
    match handler.transport {
        ReverseTunnelTransport::Tcp => run_tcp_initiator(handler).await,
        ReverseTunnelTransport::Rtp => run_rtp_initiator(handler).await,
    }
}

async fn run_tcp_initiator(
    handler: Arc<ReverseTunnelInitiatorHandler>,
) -> Result<(), ReverseTunnelSessionError> {
    let (mut stream, addr) =
        connect_concrete(&handler.responder_addr, &handler.stream_runtime).await?;
    send_registration(&mut stream, &handler).await?;
    let pair = SocketAddrPair {
        local_addr: stream.local_addr()?,
        peer_addr: addr,
    };
    let (reader, writer) = tokio::io::split(stream);
    let mut supervisor = JoinSet::new();
    let (_opener, accepter) =
        spawn_mux_no_reconnection(reader, writer, client_mux_config(), &mut supervisor);
    counter!("revtun.session.connected").increment(1);
    run_tcp_accepter(accepter, pair, handler, supervisor).await
}

async fn run_rtp_initiator(
    handler: Arc<ReverseTunnelInitiatorHandler>,
) -> Result<(), ReverseTunnelSessionError> {
    let sock_addrs = handler.responder_addr.address.to_socket_addrs().await?;
    let mut last_error = None;
    for addr in sock_addrs.iter().copied() {
        let connector_table = Arc::clone(&handler.stream_runtime.connector_table);
        let bind: rtp_mux::BindSelector = Arc::new(move |peer| connector_table.bind_addr_for(peer));
        match rtp_mux::connect_bidirectional_session(
            addr,
            rtp_mux::RtpMuxConnectorConfig::standard(bind),
        )
        .await
        {
            Ok(session) => {
                let (opener, accepter, pair, driver) = session.into_parts();
                let (reader, writer) = opener
                    .open(LaneClass::Interactive)
                    .await
                    .map_err(|error| ReverseTunnelSessionError::Mux(format!("{error:?}")))?;
                let mut registration = tokio_chacha20::stream::DuplexStream::new(reader, writer);
                send_registration(&mut registration, &handler).await?;
                drop(registration);
                drop(opener);
                counter!("revtun.session.connected").increment(1);
                return run_rtp_accepter(accepter, pair.into(), handler, driver).await;
            }
            Err(error) => last_error = Some(error),
        }
    }
    Err(last_error
        .unwrap_or_else(|| io::Error::other("responder resolved to no addresses"))
        .into())
}

async fn send_registration<Stream>(
    stream: &mut Stream,
    handler: &ReverseTunnelInitiatorHandler,
) -> Result<(), ReverseTunnelSessionError>
where
    Stream: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    timed_write_header_async(
        stream,
        &RegisterRequest {
            version: REGISTER_VERSION,
            name: Arc::clone(&handler.name),
        },
        *handler.registration_crypto.key(),
        common::STREAM_IO_TIMEOUT,
    )
    .await?;
    let validator = ValidatorRef::Replay(&handler.stream_runtime.replay_validator);
    let response: RegisterResponse = timed_read_header_async(
        stream,
        *handler.registration_crypto.key(),
        &validator,
        common::STREAM_IO_TIMEOUT,
    )
    .await?;
    response
        .result
        .map_err(ReverseTunnelSessionError::Registration)
}

async fn run_tcp_accepter(
    mut accepter: mux::StreamAccepter,
    addr: SocketAddrPair,
    handler: Arc<ReverseTunnelInitiatorHandler>,
    mut supervisor: JoinSet<MuxError>,
) -> Result<(), ReverseTunnelSessionError> {
    loop {
        tokio::select! {
            accepted = accepter.accept() => {
                let (reader, writer) =
                    accepted.map_err(|_| ReverseTunnelSessionError::Closed)?;
                dispatch_tunnel_flow(reader, writer, addr, &handler);
            }
            result = supervisor.join_next() => {
                return Err(ReverseTunnelSessionError::Mux(format!(
                    "{:?}",
                    mux_result(result)
                )));
            }
        }
    }
}

async fn run_rtp_accepter(
    mut accepter: mux::DualStreamAccepter,
    addr: SocketAddrPair,
    handler: Arc<ReverseTunnelInitiatorHandler>,
    mut driver: rtp_mux::BidirectionalSessionDriver,
) -> Result<(), ReverseTunnelSessionError> {
    loop {
        tokio::select! {
            accepted = accepter.accept() => {
                let (reader, writer, _) =
                    accepted.map_err(|_| ReverseTunnelSessionError::Closed)?;
                dispatch_tunnel_flow(reader, writer, addr, &handler);
            }
            error = &mut driver => {
                return Err(ReverseTunnelSessionError::Mux(format!("{error:?}")));
            }
        }
    }
}

fn dispatch_tunnel_flow(
    reader: mux::StreamReader,
    writer: mux::StreamWriter,
    addr: SocketAddrPair,
    handler: &Arc<ReverseTunnelInitiatorHandler>,
) {
    let conn_handler = Arc::new(TunnelMuxFlowHandler {
        stream: Arc::clone(&handler.stream_proxy),
        udp: Arc::clone(&handler.udp_proxy),
    });
    if let Err(error) = handler
        .stream_runtime
        .session_spawner
        .try_spawn(async move {
            let stream = tokio_chacha20::stream::DuplexStream::new(reader, writer);
            dispatch_mux_flow(
                stream,
                addr,
                conn_handler,
                AddressedMuxStream::new,
                "revtun.udp.flows",
            )
            .await;
            Ok(())
        })
    {
        log_rejection("reverse_tunnel", error);
    }
}

/// The mux flow handler for a reverse-tunnel session: dispatches accepted
/// streams to the initiator's stream/UDP proxy handlers exactly like the
/// mux proxy listeners do, so the wire format is identical on every mux
/// transport.
#[derive(Debug)]
struct TunnelMuxFlowHandler {
    stream: Arc<StreamProxyConnHandler>,
    udp: Arc<UdpProxyConnHandler>,
}
impl loading::HandleConn for TunnelMuxFlowHandler {}
impl StreamServerHandleConn for TunnelMuxFlowHandler {
    async fn handle_stream<Stream>(&self, stream: Stream)
    where
        Stream: IoConnection + std::fmt::Debug,
    {
        self.stream.handle_stream(stream).await;
    }
}
impl MuxProxyConnHandler for TunnelMuxFlowHandler {
    fn udp_proxy(&self) -> Option<&UdpProxyConnHandler> {
        Some(&self.udp)
    }
}

async fn connect_concrete(
    addr: &RouteAddr,
    runtime: &StreamRuntime,
) -> Result<(Box<dyn IoConnection>, SocketAddr), ReverseTunnelSessionError> {
    let sock_addrs = addr.address.to_socket_addrs().await?;
    runtime
        .connector_table
        .timed_connect_any(
            &addr.protocol,
            sock_addrs.iter().copied(),
            common::STREAM_IO_TIMEOUT,
        )
        .await
        .map_err(Into::into)
}

#[derive(Debug)]
pub struct ReverseTunnelInitiatorBuilder {
    pub(crate) key: Arc<str>,
    config: ReverseTunnelInitiatorConfig,
    runtime: Runtime,
}
impl ReverseTunnelInitiatorBuilder {
    pub(crate) fn new(
        config: ReverseTunnelInitiatorConfig,
        runtime: Runtime,
    ) -> Result<Self, BuildError> {
        validate_reverse_tunnel_name(&config.name).map_err(|_| BuildError::InvalidName)?;
        let transport = initiator_transport(&config.responder_addr.0)?;
        let key = Arc::from(format!("{}://{}", transport.protocol(), config.name));
        Ok(Self {
            key,
            config,
            runtime,
        })
    }
    pub(crate) fn handler(self) -> Result<ReverseTunnelInitiatorHandler, BuildError> {
        let transport = initiator_transport(&self.config.responder_addr.0)?;
        let registration_crypto = self
            .config
            .header_key
            .build()
            .map_err(|error| BuildError::HeaderCrypto(error.source.to_string()))?;
        let payload_crypto = build_payload_crypto(self.config.payload_key)?;
        let stream_proxy = Arc::new(StreamProxyConnHandler::new(
            registration_crypto.clone(),
            payload_crypto.clone(),
            self.runtime.stream.clone(),
            self.key.clone(),
            self.config.allow_loopback,
        ));
        let udp_proxy = Arc::new(UdpProxyConnHandler::new(
            registration_crypto.clone(),
            payload_crypto,
            self.runtime.udp,
            self.config.allow_loopback,
        ));
        Ok(ReverseTunnelInitiatorHandler {
            name: self.config.name,
            responder_addr: self.config.responder_addr.0,
            transport,
            registration_crypto,
            stream_proxy,
            udp_proxy,
            stream_runtime: self.runtime.stream,
        })
    }
}

fn build_payload_crypto(
    payload_key: Option<tokio_chacha20::config::ConfigBuilder>,
) -> Result<Option<tokio_chacha20::config::Config>, BuildError> {
    payload_key
        .map(|key| key.build())
        .transpose()
        .map_err(|error| BuildError::PayloadCrypto(error.source.to_string()))
}
impl loading::Build for ReverseTunnelInitiatorBuilder {
    type ConnHandler = ReverseTunnelInitiatorHandler;
    type Server = ReverseTunnelInitiator;
    type Err = BuildError;
    async fn build_server(self) -> Result<Self::Server, Self::Err> {
        Ok(ReverseTunnelInitiator {
            handler: self.handler()?,
        })
    }
    fn build_conn_handler(self) -> Result<Self::ConnHandler, Self::Err> {
        self.handler()
    }
    fn key(&self) -> &Arc<str> {
        &self.key
    }
}

pub(crate) fn initiator_transport(addr: &RouteAddr) -> Result<ReverseTunnelTransport, BuildError> {
    match addr.protocol.as_ref() {
        "tcp" => Ok(ReverseTunnelTransport::Tcp),
        "rtpmux" => Ok(ReverseTunnelTransport::Rtp),
        protocol => Err(BuildError::UnsupportedPhysicalTransport(protocol.into())),
    }
}
