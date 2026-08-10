use std::{
    collections::HashSet,
    convert::Infallible,
    io,
    net::SocketAddr,
    sync::Arc,
    time::{Duration, Instant},
};

use ae::anti_replay::ValidatorRef;
use async_trait::async_trait;
use common::{
    config::Merge,
    error::{AnyError, AnyResult},
    header::codec::{AsHeader, CodecError, timed_read_header_async, timed_write_header_async},
    loading,
    proto::{
        addr::{
            REVERSE_TUNNEL_RTP_PROTOCOL, REVERSE_TUNNEL_TCP_PROTOCOL, ReverseTunnelTransport,
            RouteAddr, RouteAddrStr, validate_reverse_tunnel_name,
        },
        conn_handler::{stream::StreamProxyConnHandler, udp::UdpProxyConnHandler},
        connect::{
            stream::NamedStreamConnect,
            udp::{NamedUdpConnect, UdpConnection},
        },
        context::{Runtime, StreamRuntime, UdpRuntime},
    },
    session::log_rejection,
    stream::{ConnParts, HasIoAddr, StreamServerHandleConn},
};
use metrics::{counter, gauge};
use mux::{LaneClass, MuxError, spawn_mux_no_reconnection};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::{net::TcpListener, task::JoinSet};
use tracing::{info, warn};

use crate::stream::streams::{
    mux::{
        AddressedMuxStream, MuxFlowKind, MuxProxyConnHandler, SocketAddrPair, client_mux_config,
        dispatch_mux_flow, server_mux_config, write_flow_kind,
    },
    tcp::proxy_server::AddressedTcpStream,
};

const REGISTER_VERSION: u16 = 2;
const INITIAL_RECONNECT_DELAY: Duration = Duration::from_millis(250);
const MAX_RECONNECT_DELAY: Duration = Duration::from_secs(30);
const STABLE_SESSION: Duration = Duration::from_secs(30);

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct ReverseTunnelConfig {
    #[serde(default)]
    pub initiator: Vec<ReverseTunnelInitiatorConfig>,
    #[serde(default)]
    pub responder: Vec<ReverseTunnelResponderConfig>,
}
impl Merge for ReverseTunnelConfig {
    type Error = Infallible;
    fn merge(mut self, other: Self) -> Result<Self, Self::Error> {
        self.initiator.extend(other.initiator);
        self.responder.extend(other.responder);
        Ok(self)
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReverseTunnelInitiatorConfig {
    pub name: Arc<str>,
    pub responder_addr: RouteAddrStr,
    pub header_key: tokio_chacha20::config::ConfigBuilder,
    #[serde(default)]
    pub payload_key: Option<tokio_chacha20::config::ConfigBuilder>,
    #[serde(default)]
    pub allow_loopback: bool,
    #[serde(default)]
    pub fec: bool,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReverseTunnelResponderConfig {
    pub listen_addr: RouteAddrStr,
    pub header_key: tokio_chacha20::config::ConfigBuilder,
    #[serde(default)]
    pub fec: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct RegisterRequest {
    version: u16,
    name: Arc<str>,
}
impl AsHeader for RegisterRequest {}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct RegisterResponse {
    result: Result<(), RegisterError>,
}
impl AsHeader for RegisterResponse {}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Error)]
enum RegisterError {
    #[error("unsupported reverse tunnel protocol version")]
    UnsupportedVersion,
    #[error("invalid reverse tunnel name")]
    InvalidName,
}

#[derive(Debug, Clone)]
enum RegisteredOpener {
    Tcp(mux::StreamOpener),
    Rtp(mux::DualStreamOpener),
}

impl RegisteredOpener {
    async fn open(&self) -> io::Result<(mux::StreamReader, mux::StreamWriter)> {
        match self {
            Self::Tcp(opener) => opener
                .open()
                .await
                .map_err(|error| io::Error::other(format!("{error:?}"))),
            Self::Rtp(opener) => opener
                .open(LaneClass::Interactive)
                .await
                .map_err(|error| io::Error::other(format!("{error:?}"))),
        }
    }
}

#[derive(Debug)]
struct RegisteredTunnel {
    opener: RegisteredOpener,
    addr: SocketAddrPair,
    connected_at: Instant,
}

#[derive(Debug)]
struct RegisteredUdpTunnel {
    opener: RegisteredOpener,
    addr: SocketAddrPair,
    connected_at: Instant,
}
#[async_trait]
impl NamedStreamConnect for RegisteredTunnel {
    async fn connect(&self) -> io::Result<Box<dyn ConnParts>> {
        let (reader, mut writer) = self.opener.open().await?;
        write_flow_kind(&mut writer, MuxFlowKind::Stream).await?;
        counter!("revtun.stream.opened").increment(1);
        let stream = AddressedMuxStream::new(
            tokio_chacha20::stream::DuplexStream::new(reader, writer),
            self.addr,
        );
        Ok(Box::new(stream))
    }
    fn session_stats(&self) -> Option<String> {
        Some(format!(
            "peer={},uptime={:?}",
            self.addr.peer_addr,
            self.connected_at.elapsed()
        ))
    }
}

#[async_trait]
impl NamedUdpConnect for RegisteredUdpTunnel {
    async fn connect(&self) -> io::Result<UdpConnection> {
        let (reader, mut writer) = self.opener.open().await?;
        write_flow_kind(&mut writer, MuxFlowKind::Udp).await?;
        counter!("revtun.udp.opened").increment(1);
        Ok(UdpConnection::mux_io(
            reader,
            writer,
            self.addr.local_addr,
            self.addr.peer_addr,
        ))
    }
    fn session_stats(&self) -> Option<String> {
        Some(format!(
            "peer={}, uptime={:?}",
            self.addr.peer_addr,
            self.connected_at.elapsed()
        ))
    }
}

#[derive(Debug)]
pub struct ReverseTunnelInitiatorHandler {
    name: Arc<str>,
    responder_addr: RouteAddr,
    transport: ReverseTunnelTransport,
    registration_crypto: tokio_chacha20::config::Config,
    stream_proxy: Arc<StreamProxyConnHandler>,
    udp_proxy: Arc<UdpProxyConnHandler>,
    stream_runtime: StreamRuntime,
    fec: bool,
}
impl loading::HandleConn for ReverseTunnelInitiatorHandler {}

#[derive(Debug)]
pub struct ReverseTunnelResponderHandler {
    registration_crypto: tokio_chacha20::config::Config,
    stream_runtime: StreamRuntime,
    udp_runtime: UdpRuntime,
}
impl loading::HandleConn for ReverseTunnelResponderHandler {}

#[derive(Debug)]
pub struct ReverseTunnelInitiator {
    handler: ReverseTunnelInitiatorHandler,
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
            rtp_mux::RtpMuxConnectorConfig::standard(bind, handler.fec),
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
        Stream: ConnParts + std::fmt::Debug,
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
) -> Result<(Box<dyn ConnParts>, SocketAddr), ReverseTunnelSessionError> {
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

fn mux_result(result: Option<Result<MuxError, tokio::task::JoinError>>) -> MuxError {
    match result {
        Some(result) => result.unwrap(),
        None => MuxError::TaskStopped { task: "revtun" },
    }
}

impl ReverseTunnelResponderHandler {
    async fn register<Stream>(
        &self,
        stream: &mut Stream,
    ) -> Result<Arc<str>, ReverseTunnelSessionError>
    where
        Stream: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
    {
        let validator = ValidatorRef::Replay(&self.stream_runtime.replay_validator);
        let request: RegisterRequest = timed_read_header_async(
            stream,
            *self.registration_crypto.key(),
            &validator,
            common::STREAM_IO_TIMEOUT,
        )
        .await?;
        let result = if request.version != REGISTER_VERSION {
            Err(RegisterError::UnsupportedVersion)
        } else if validate_reverse_tunnel_name(&request.name).is_err() {
            Err(RegisterError::InvalidName)
        } else {
            Ok(())
        };
        timed_write_header_async(
            stream,
            &RegisterResponse {
                result: result.clone(),
            },
            *self.registration_crypto.key(),
            common::STREAM_IO_TIMEOUT,
        )
        .await?;
        result.map_err(ReverseTunnelSessionError::Registration)?;
        Ok(request.name)
    }

    async fn handle_tcp(
        self: Arc<Self>,
        mut stream: AddressedTcpStream,
    ) -> Result<(), ReverseTunnelSessionError> {
        let name = self.register(&mut stream).await?;
        let addr = SocketAddrPair {
            local_addr: stream.local_addr()?,
            peer_addr: stream.peer_addr()?,
        };
        let (reader, writer) = tokio::io::split(stream);
        let mut supervisor = JoinSet::new();
        let (opener, _accepter) =
            spawn_mux_no_reconnection(reader, writer, server_mux_config(), &mut supervisor);
        let opener = RegisteredOpener::Tcp(opener);
        let connected_at = Instant::now();
        let connector = Arc::new(RegisteredTunnel {
            opener: opener.clone(),
            addr,
            connected_at,
        });
        let _registration = self.stream_runtime.connector_table.register_named(
            REVERSE_TUNNEL_TCP_PROTOCOL.into(),
            Arc::clone(&name),
            connector,
        );
        let udp_connector = Arc::new(RegisteredUdpTunnel {
            opener,
            addr,
            connected_at,
        });
        let _udp_registration = self.udp_runtime.connector.register_named(
            REVERSE_TUNNEL_TCP_PROTOCOL.into(),
            Arc::clone(&name),
            udp_connector,
        );
        let _active = registered(&name, ReverseTunnelTransport::Tcp);
        let error = mux_result(supervisor.join_next().await);
        Err(ReverseTunnelSessionError::Mux(format!("{error:?}")))
    }

    async fn handle_rtp(
        self: Arc<Self>,
        session: rtp_mux::BidirectionalSession,
    ) -> Result<(), ReverseTunnelSessionError> {
        let (opener, mut accepter, addr, mut driver) = session.into_parts();
        let (reader, writer, _) = accepter
            .accept()
            .await
            .map_err(|_| ReverseTunnelSessionError::Closed)?;
        let mut registration = tokio_chacha20::stream::DuplexStream::new(reader, writer);
        let name = self.register(&mut registration).await?;
        drop(registration);
        drop(accepter);
        let addr: SocketAddrPair = addr.into();
        let opener = RegisteredOpener::Rtp(opener);
        let connected_at = Instant::now();
        let connector = Arc::new(RegisteredTunnel {
            opener: opener.clone(),
            addr,
            connected_at,
        });
        let _registration = self.stream_runtime.connector_table.register_named(
            REVERSE_TUNNEL_RTP_PROTOCOL.into(),
            Arc::clone(&name),
            connector,
        );
        let udp_connector = Arc::new(RegisteredUdpTunnel {
            opener,
            addr,
            connected_at,
        });
        let _udp_registration = self.udp_runtime.connector.register_named(
            REVERSE_TUNNEL_RTP_PROTOCOL.into(),
            Arc::clone(&name),
            udp_connector,
        );
        let _active = registered(&name, ReverseTunnelTransport::Rtp);
        let error = (&mut driver).await;
        Err(ReverseTunnelSessionError::Mux(format!("{error:?}")))
    }
}

fn registered(name: &str, transport: ReverseTunnelTransport) -> ActiveSessionGauge {
    info!(
        name,
        protocol = transport.protocol(),
        "Reverse tunnel registered"
    );
    counter!("revtun.registration.accepted").increment(1);
    gauge!("revtun.active_sessions").increment(1.);
    ActiveSessionGauge
}

struct ActiveSessionGauge;
impl Drop for ActiveSessionGauge {
    fn drop(&mut self) {
        gauge!("revtun.active_sessions").decrement(1.);
    }
}

#[derive(Debug)]
pub struct TcpReverseTunnelResponder {
    listener: TcpListener,
    handler: ReverseTunnelResponderHandler,
}
impl loading::Serve for TcpReverseTunnelResponder {
    type ConnHandler = ReverseTunnelResponderHandler;
    async fn serve(
        self,
        replacement_rx: loading::ReplaceConnHandlerRx<Self::ConnHandler>,
    ) -> AnyResult {
        let addr = self.listener.local_addr()?;
        let listener = self.listener;
        let mut state = ();
        common::serve_loop::serve_loop(
            addr,
            Arc::new(self.handler),
            replacement_rx,
            |_| {},
            || listener.accept(),
            |_state, (stream, _), handler| {
                Box::pin(async move {
                    let spawner = handler.stream_runtime.session_spawner.clone();
                    let _ = spawner
                        .spawn(async move {
                            if let Err(error) = handler.handle_tcp(AddressedTcpStream(stream)).await
                            {
                                warn!(?error, "TCP reverse tunnel session failed");
                            }
                            Ok(())
                        })
                        .await;
                })
            },
            &mut state,
            |_| Box::pin(std::future::pending()),
            common::serve_loop::ServeLoopConfig {
                label: "revtun_tcp",
                counter_name: Some("revtun.tcp.accepted"),
                counts_dispatch_errors: false,
            },
        )
        .await?;
        Ok(())
    }
}

#[derive(Debug)]
pub struct RtpReverseTunnelResponder {
    server: rtp_mux::RtpMuxServer,
    handler: ReverseTunnelResponderHandler,
    session_spawner: common::session::SessionSpawner,
}
impl loading::Serve for RtpReverseTunnelResponder {
    type ConnHandler = ReverseTunnelResponderHandler;
    async fn serve(
        self,
        mut replacement_rx: loading::ReplaceConnHandlerRx<Self::ConnHandler>,
    ) -> AnyResult {
        let handler = Arc::new(std::sync::RwLock::new(Arc::new(self.handler)));
        let handler_for_session = Arc::clone(&handler);
        let session_spawner = self.session_spawner.clone();
        let rtp_session_spawner = rtp_mux::SessionSpawner::new({
            let session_spawner = session_spawner.clone();
            move |session| {
                if let Err(error) = session_spawner.try_spawn(async move {
                    session.await;
                    Ok(())
                }) {
                    log_rejection("revtun_rtp_mux", error);
                }
            }
        });
        let serving = self
            .server
            .serve_sessions(rtp_session_spawner, move |session| {
                let handler = handler_for_session.read().unwrap().clone();
                let session_spawner = session_spawner.clone();
                if let Err(error) = session_spawner.try_spawn(async move {
                    if let Err(error) = handler.handle_rtp(session).await {
                        warn!(?error, "RTP reverse tunnel session failed");
                    }
                    Ok(())
                }) {
                    log_rejection("revtun_rtp_session", error);
                }
            });
        tokio::pin!(serving);
        loop {
            tokio::select! {
                result = &mut serving => return result.map_err(Into::into),
                replacement = replacement_rx.recv() => {
                    match replacement {
                        Ok(Some(new_handler)) => {
                            *handler.write().unwrap() = new_handler;
                        }
                        Ok(None) => {}
                        Err(()) => return Ok(()),
                    }
                }
            }
        }
    }
}

#[derive(Debug)]
pub struct ReverseTunnelInitiatorBuilder {
    key: Arc<str>,
    config: ReverseTunnelInitiatorConfig,
    runtime: Runtime,
}
impl ReverseTunnelInitiatorBuilder {
    fn new(config: ReverseTunnelInitiatorConfig, runtime: Runtime) -> Result<Self, BuildError> {
        validate_reverse_tunnel_name(&config.name).map_err(|_| BuildError::InvalidName)?;
        let transport = initiator_transport(&config.responder_addr.0)?;
        let key = Arc::from(format!("{}://{}", transport.protocol(), config.name));
        Ok(Self {
            key,
            config,
            runtime,
        })
    }
    fn handler(self) -> Result<ReverseTunnelInitiatorHandler, BuildError> {
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
            fec: self.config.fec,
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

#[derive(Debug)]
pub struct TcpReverseTunnelResponderBuilder {
    key: Arc<str>,
    listen_addr: RouteAddr,
    header_key: tokio_chacha20::config::ConfigBuilder,
    runtime: Runtime,
}
impl loading::Build for TcpReverseTunnelResponderBuilder {
    type ConnHandler = ReverseTunnelResponderHandler;
    type Server = TcpReverseTunnelResponder;
    type Err = BuildError;
    async fn build_server(self) -> Result<Self::Server, Self::Err> {
        let listener = TcpListener::bind(self.listen_addr.address.to_string()).await?;
        Ok(TcpReverseTunnelResponder {
            listener,
            handler: self.build_conn_handler()?,
        })
    }
    fn build_conn_handler(self) -> Result<Self::ConnHandler, Self::Err> {
        responder_handler(self.header_key, self.runtime)
    }
    fn key(&self) -> &Arc<str> {
        &self.key
    }
}

#[derive(Debug)]
pub struct RtpReverseTunnelResponderBuilder {
    key: Arc<str>,
    listen_addr: RouteAddr,
    header_key: tokio_chacha20::config::ConfigBuilder,
    runtime: Runtime,
    fec: bool,
}
impl loading::Build for RtpReverseTunnelResponderBuilder {
    type ConnHandler = ReverseTunnelResponderHandler;
    type Server = RtpReverseTunnelResponder;
    type Err = BuildError;
    async fn build_server(self) -> Result<Self::Server, Self::Err> {
        let server =
            rtp_mux::RtpMuxServer::bind(self.listen_addr.address.to_string(), self.fec).await?;
        let session_spawner = self.runtime.session_spawner.clone();
        Ok(RtpReverseTunnelResponder {
            server,
            handler: self.build_conn_handler()?,
            session_spawner,
        })
    }
    fn build_conn_handler(self) -> Result<Self::ConnHandler, Self::Err> {
        responder_handler(self.header_key, self.runtime)
    }
    fn key(&self) -> &Arc<str> {
        &self.key
    }
}

fn responder_handler(
    header_key: tokio_chacha20::config::ConfigBuilder,
    runtime: Runtime,
) -> Result<ReverseTunnelResponderHandler, BuildError> {
    let registration_crypto = header_key
        .build()
        .map_err(|error| BuildError::HeaderCrypto(error.source.to_string()))?;
    Ok(ReverseTunnelResponderHandler {
        registration_crypto,
        stream_runtime: runtime.stream,
        udp_runtime: runtime.udp,
    })
}

fn initiator_transport(addr: &RouteAddr) -> Result<ReverseTunnelTransport, BuildError> {
    match addr.protocol.as_ref() {
        "tcp" => Ok(ReverseTunnelTransport::Tcp),
        "rtpmux" => Ok(ReverseTunnelTransport::Rtp),
        protocol => Err(BuildError::UnsupportedPhysicalTransport(protocol.into())),
    }
}

fn responder_transport(addr: &RouteAddr) -> Result<ReverseTunnelTransport, BuildError> {
    initiator_transport(addr)
}

#[derive(Debug, Error)]
pub enum BuildError {
    #[error("invalid reverse tunnel name")]
    InvalidName,
    #[error("unsupported reverse tunnel transport `{0}`; expected `tcp` or `rtpmux`")]
    UnsupportedPhysicalTransport(Arc<str>),
    #[error("header crypto: {0}")]
    HeaderCrypto(String),
    #[error("payload crypto: {0}")]
    PayloadCrypto(String),
    #[error("failed to bind reverse tunnel responder: {0}")]
    Bind(#[from] io::Error),
    #[error("duplicate reverse tunnel configuration key `{0}`")]
    DuplicateKey(Arc<str>),
}

#[derive(Debug, Error)]
enum ReverseTunnelSessionError {
    #[error("I/O error: {0}")]
    Io(#[from] io::Error),
    #[error("registration codec error: {0}")]
    Codec(#[from] CodecError),
    #[error("registration rejected: {0}")]
    Registration(RegisterError),
    #[error("mux error: {0}")]
    Mux(String),
    #[error("reverse tunnel session closed")]
    Closed,
}

#[derive(Debug)]
pub struct ReverseTunnelLoader {
    initiator: loading::Loader<ReverseTunnelInitiatorHandler>,
    tcp_responder: loading::Loader<ReverseTunnelResponderHandler>,
    rtp_responder: loading::Loader<ReverseTunnelResponderHandler>,
}
impl ReverseTunnelLoader {
    pub fn new() -> Self {
        Self {
            initiator: loading::Loader::new(),
            tcp_responder: loading::Loader::new(),
            rtp_responder: loading::Loader::new(),
        }
    }
    pub fn commit(
        &mut self,
        tasks: &mut JoinSet<AnyResult>,
        prepared: PreparedReverseTunnel,
    ) -> AnyResult {
        self.initiator.commit(tasks, prepared.initiator)?;
        self.tcp_responder.commit(tasks, prepared.tcp_responder)?;
        self.rtp_responder.commit(tasks, prepared.rtp_responder)?;
        Ok(())
    }
}
impl Default for ReverseTunnelLoader {
    fn default() -> Self {
        Self::new()
    }
}

pub struct PreparedReverseTunnel {
    initiator: loading::PreparedOps<ReverseTunnelInitiatorHandler>,
    tcp_responder: loading::PreparedOps<ReverseTunnelResponderHandler>,
    rtp_responder: loading::PreparedOps<ReverseTunnelResponderHandler>,
}

pub async fn prepare(
    config: ReverseTunnelConfig,
    loader: &ReverseTunnelLoader,
    runtime: Runtime,
) -> Result<PreparedReverseTunnel, AnyError> {
    let mut keys = HashSet::new();
    let mut initiators = Vec::with_capacity(config.initiator.len());
    for config in config.initiator {
        let builder = ReverseTunnelInitiatorBuilder::new(config, runtime.clone())?;
        if !keys.insert(builder.key.clone()) {
            return Err(BuildError::DuplicateKey(builder.key).into());
        }
        initiators.push(builder);
    }
    let mut responder_keys = HashSet::new();
    let mut tcp_responders = Vec::new();
    let mut rtp_responders = Vec::new();
    for config in config.responder {
        let listen_addr = config.listen_addr.0;
        let transport = responder_transport(&listen_addr)?;
        let key: Arc<str> = Arc::from(listen_addr.to_string());
        if !responder_keys.insert(key.clone()) {
            return Err(BuildError::DuplicateKey(key).into());
        }
        match transport {
            ReverseTunnelTransport::Tcp => tcp_responders.push(TcpReverseTunnelResponderBuilder {
                key,
                listen_addr,
                header_key: config.header_key,
                runtime: runtime.clone(),
            }),
            ReverseTunnelTransport::Rtp => rtp_responders.push(RtpReverseTunnelResponderBuilder {
                key,
                listen_addr,
                header_key: config.header_key,
                runtime: runtime.clone(),
                fec: config.fec,
            }),
        }
    }
    Ok(PreparedReverseTunnel {
        initiator: loader.initiator.prepare(initiators).await?,
        tcp_responder: loader.tcp_responder.prepare(tcp_responders).await?,
        rtp_responder: loader.rtp_responder.prepare(rtp_responders).await?,
    })
}

#[cfg(test)]
#[allow(clippy::disallowed_methods)]
mod tests {
    use super::*;
    use ae::anti_replay::{ReplayValidator, TimeValidator};
    use common::{
        anti_replay::{VALIDATOR_CAPACITY, VALIDATOR_TIME_FRAME},
        connect::{ConnectorConfig, ConnectorResetSignal},
        loading::Serve,
        notify::Notify,
        proto::{
            client::{stream, udp::UdpProxyClient},
            connect::udp::UdpConnector,
            context::{Runtime, UdpRuntime},
        },
        route::{ConnChain, ConnConfig},
        stream::pool::StreamConnPool,
    };
    use std::sync::RwLock;
    use swap::Swap;
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::TcpListener,
    };
    use tokio_chacha20::config::ConfigBuilder;

    use crate::stream::connect::build_concrete_stream_connector_table;

    /// Actively-polled scope of test-owned background tasks. The test body
    /// runs through [`TestScope::run`], which races it against `join_next()`
    /// on the scope, so a background task that panics fails the test
    /// immediately instead of being observed only when the scope is dropped.
    /// A `spawn_required` actor completing before the body does is equally a
    /// failure (the wrapper turns the completion into a panic); tasks that
    /// end normally are drained silently (e.g. the origin server finishing
    /// its single accept). Dropping the scope remains the abort backstop for
    /// tasks still running when the body completes.
    struct TestScope {
        tasks: JoinSet<()>,
    }
    impl TestScope {
        fn new() -> Self {
            Self {
                tasks: JoinSet::new(),
            }
        }
        fn spawn(&mut self, task: impl std::future::Future<Output = ()> + Send + 'static) {
            self.tasks.spawn(task);
        }
        /// Spawn a long-lived actor that must stay alive for the whole
        /// [`TestScope::run`] body (runtime actors, the responder server,
        /// the initiator). A normal completion while the body is still
        /// running panics the test with a message naming the task; a panic
        /// inside the future propagates unchanged.
        fn spawn_required(
            &mut self,
            name: &'static str,
            task: impl std::future::Future<Output = ()> + Send + 'static,
        ) {
            self.tasks.spawn(async move {
                task.await;
                panic!("required task '{name}' exited before the test body completed");
            });
        }
        async fn run<F: std::future::Future>(mut self, body: F) -> F::Output {
            tokio::pin!(body);
            loop {
                tokio::select! {
                    biased;
                    joined = self.tasks.join_next(), if !self.tasks.is_empty() => {
                        // A background task exited before the body. Re-raise
                        // any panic it surfaced immediately; a normal
                        // completion is a legitimate shutdown (e.g. the
                        // origin server finishing its single accept) and is
                        // drained silently.
                        let joined = joined.expect("background task exists");
                        joined.unwrap();
                    }
                    value = &mut body => {
                        // The body completed. Drain tasks that exited in the
                        // same poll cycle so a required actor that ended
                        // right as the body finished still fails the test.
                        while let Some(joined) = self.tasks.try_join_next() {
                            joined.unwrap();
                        }
                        return value;
                    }
                }
            }
        }
    }

    #[test]
    fn config_has_no_destination_field() {
        let config: ReverseTunnelConfig = serde_json::from_str(
            r#"{
                "initiator": [{ "name": "private-a", "responder_addr": "tcp://127.0.0.1:7000", "header_key": "aGVsbG8" }],
                "responder": [{ "listen_addr": "rtpmux://127.0.0.1:7000", "header_key": "aGVsbG8" }]
            }"#,
        )
        .unwrap();
        assert_eq!(config.initiator[0].name.as_ref(), "private-a");
        assert_eq!(
            config.responder[0].listen_addr.0.protocol.as_ref(),
            "rtpmux"
        );
    }

    #[test]
    fn unsupported_physical_transports_are_rejected() {
        let addr: RouteAddr = "rtp://127.0.0.1:7000".parse().unwrap();
        assert!(matches!(
            initiator_transport(&addr),
            Err(BuildError::UnsupportedPhysicalTransport(_))
        ));
    }

    #[test]
    fn responder_rejects_payload_key() {
        let error = serde_json::from_str::<ReverseTunnelConfig>(
            r#"{
                "responder": [{ "listen_addr": "tcp://127.0.0.1:7000", "header_key": "aGVsbG8", "payload_key": "aGVsbG8" }]
            }"#,
        )
        .unwrap_err();
        assert!(error.to_string().contains("payload_key"), "{error}");
    }

    fn initiator_config(
        name: &str,
        responder_addr: RouteAddr,
        header_key: ConfigBuilder,
        payload_key: Option<ConfigBuilder>,
    ) -> ReverseTunnelInitiatorConfig {
        ReverseTunnelInitiatorConfig {
            name: name.into(),
            responder_addr: RouteAddrStr(responder_addr),
            header_key,
            payload_key,
            allow_loopback: true,
            fec: false,
        }
    }

    async fn test_stream_runtime(
        tasks: &mut TestScope,
        udp_connector: &UdpConnector,
        connector_config: Arc<RwLock<ConnectorConfig>>,
    ) -> (StreamRuntime, common::session::SessionSpawner) {
        let (session_spawner, mut session_rx) = common::session::SessionSpawner::channel();
        tasks.spawn_required("session spawner", async move {
            let mut sessions = JoinSet::new();
            loop {
                tokio::select! {
                    Some(session) = session_rx.recv() => {
                        sessions.spawn(session);
                    }
                    Some(result) = sessions.join_next() => {
                        result.unwrap().unwrap();
                    }
                    else => break,
                }
            }
        });
        let reset = ConnectorResetSignal(Notify::new());
        let mut connector_drivers = JoinSet::new();
        let connector_table = Arc::new(build_concrete_stream_connector_table(
            connector_config,
            reset,
            &mut connector_drivers,
            udp_connector,
        ));
        tasks.spawn_required("connector drivers", async move {
            while let Some(result) = connector_drivers.join_next().await {
                result.unwrap().unwrap();
            }
        });
        let (retention_actor, retention) = common::retention::RetentionActor::new();
        tasks.spawn_required("retention actor", async move {
            retention_actor.run().await;
        });
        (
            StreamRuntime {
                session_table: None,
                pool: Swap::new(StreamConnPool::empty()),
                connector_table,
                replay_validator: Arc::new(ReplayValidator::new(
                    VALIDATOR_TIME_FRAME,
                    VALIDATOR_CAPACITY,
                )),
                session_spawner: session_spawner.clone(),
                retention,
            },
            session_spawner,
        )
    }

    async fn test_runtime(tasks: &mut TestScope) -> (Runtime, common::session::SessionSpawner) {
        let connector_config = Arc::new(RwLock::new(ConnectorConfig::default()));
        let udp_connector = Arc::new(UdpConnector::new(Arc::clone(&connector_config)));
        let (stream, session_spawner) =
            test_stream_runtime(tasks, &udp_connector, connector_config).await;
        let runtime = Runtime {
            stream: stream.clone(),
            udp: UdpRuntime {
                session_table: None,
                time_validator: Arc::new(TimeValidator::new(VALIDATOR_TIME_FRAME)),
                connector: udp_connector,
                session_spawner: session_spawner.clone(),
                retention: stream.retention.clone(),
            },
            session_spawner: session_spawner.clone(),
        };
        (runtime, session_spawner)
    }

    async fn origin(tasks: &mut TestScope) -> SocketAddr {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        // Transient actor: it handles ONE ping/pong exchange and then
        // completes mid-body, so it is a plain spawn (a normal completion is
        // drained silently) rather than `spawn_required`; a panic inside
        // still fails the test through the scope's join reaping.
        tasks.spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = [0; 4];
            stream.read_exact(&mut request).await.unwrap();
            assert_eq!(&request, b"ping");
            stream.write_all(b"pong").await.unwrap();
        });
        addr
    }

    fn initiator_handler(
        name: &str,
        responder_addr: RouteAddr,
        transport: ReverseTunnelTransport,
        crypto: tokio_chacha20::config::Config,
        runtime: Runtime,
    ) -> ReverseTunnelInitiatorHandler {
        let virtual_addr: Arc<str> = Arc::from(format!("{}://{name}", transport.protocol()));
        ReverseTunnelInitiatorHandler {
            name: name.into(),
            responder_addr,
            transport,
            registration_crypto: crypto.clone(),
            stream_proxy: Arc::new(StreamProxyConnHandler::new(
                crypto.clone(),
                None,
                runtime.stream.clone(),
                virtual_addr,
                true,
            )),
            udp_proxy: Arc::new(UdpProxyConnHandler::new(crypto, None, runtime.udp, true)),
            stream_runtime: runtime.stream,
            fec: false,
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn invalid_payload_key_is_reported_as_payload_crypto() {
        let mut scope = TestScope::new();
        let (runtime, _session_spawner) = test_runtime(&mut scope).await;
        let config = initiator_config(
            "private-a",
            "tcp://127.0.0.1:7000".parse().unwrap(),
            ConfigBuilder("aGVsbG8".into()),
            Some(ConfigBuilder("c2VjcmV0LXByb3h5LWtleQ!!".into())),
        );
        scope
            .run(async {
                let error = ReverseTunnelInitiatorBuilder::new(config, runtime)
                    .unwrap()
                    .handler()
                    .unwrap_err();
                assert!(matches!(error, BuildError::PayloadCrypto(_)), "{error}");
                assert!(!matches!(error, BuildError::HeaderCrypto(_)), "{error}");
            })
            .await;
    }

    async fn verify_reverse_proxy_hop_with_payload(transport: ReverseTunnelTransport) {
        let mut scope = TestScope::new();
        let (runtime, session_spawner) = test_runtime(&mut scope).await;
        let header_key = "aGVsbG8";
        let payload_key = "cGF5bG9hZA";
        let header_crypto = ConfigBuilder(header_key.into()).build().unwrap();
        let payload_crypto = ConfigBuilder(payload_key.into()).build().unwrap();
        let responder_handler = ReverseTunnelResponderHandler {
            registration_crypto: header_crypto.clone(),
            stream_runtime: runtime.stream.clone(),
            udp_runtime: runtime.udp.clone(),
        };
        let responder_addr = match transport {
            ReverseTunnelTransport::Tcp => {
                let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
                let addr = listener.local_addr().unwrap();
                let server = TcpReverseTunnelResponder {
                    listener,
                    handler: responder_handler,
                };
                scope.spawn_required("tcp responder server", async move {
                    let (_tx, rx) = loading::replace_conn_handler_channel();
                    server.serve(rx).await.unwrap();
                });
                format!("tcp://{addr}").parse().unwrap()
            }
            ReverseTunnelTransport::Rtp => {
                let server = rtp_mux::RtpMuxServer::bind("127.0.0.1:0", false)
                    .await
                    .unwrap();
                let addr = server.listener().local_addr();
                let server = RtpReverseTunnelResponder {
                    server,
                    handler: responder_handler,
                    session_spawner,
                };
                scope.spawn_required("rtp responder server", async move {
                    let (_tx, rx) = loading::replace_conn_handler_channel();
                    server.serve(rx).await.unwrap();
                });
                format!("rtpmux://{addr}").parse().unwrap()
            }
        };
        let config = initiator_config(
            "private-a",
            responder_addr,
            ConfigBuilder(header_key.into()),
            Some(ConfigBuilder(payload_key.into())),
        );
        let initiator = ReverseTunnelInitiator {
            handler: ReverseTunnelInitiatorBuilder::new(config, runtime.clone())
                .unwrap()
                .handler()
                .unwrap(),
        };
        scope.spawn_required("initiator", async move {
            let (_tx, rx) = loading::replace_conn_handler_channel();
            initiator.serve(rx).await.unwrap();
        });
        let origin_addr = origin(&mut scope).await;
        let reverse_addr: RouteAddr = format!("{}://private-a", transport.protocol())
            .parse()
            .unwrap();
        let chain = [ConnConfig {
            address: reverse_addr,
            header_crypto: header_crypto.clone(),
            payload_crypto: Some(payload_crypto.clone()),
        }];
        let destination: RouteAddr = format!("tcp://{origin_addr}").parse().unwrap();
        scope
            .run(async {
                let mut stream = tokio::time::timeout(
                    Duration::from_secs(10),
                    stream::establish(&chain, destination, &runtime.stream),
                )
                .await
                .expect("reverse tunnel did not register")
                .unwrap()
                .stream;
                tokio::time::timeout(Duration::from_secs(10), async {
                    stream.write_all(b"ping").await.unwrap();
                    let mut response = [0; 4];
                    stream.read_exact(&mut response).await.unwrap();
                    assert_eq!(&response, b"pong");
                })
                .await
                .expect("encrypted reverse proxy stream stalled");
            })
            .await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn tcp_reverse_tunnel_encrypts_payload() {
        verify_reverse_proxy_hop_with_payload(ReverseTunnelTransport::Tcp).await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn rtp_reverse_tunnel_encrypts_payload() {
        verify_reverse_proxy_hop_with_payload(ReverseTunnelTransport::Rtp).await;
    }

    async fn verify_reverse_udp_proxy_hop(
        transport: ReverseTunnelTransport,
        behind_regular_udp_hop: bool,
    ) {
        let mut scope = TestScope::new();
        let (runtime, session_spawner) = test_runtime(&mut scope).await;
        let header_key = "aGVsbG8";
        let payload_key = "cGF5bG9hZA";
        let header_crypto = ConfigBuilder(header_key.into()).build().unwrap();
        let payload_crypto = ConfigBuilder(payload_key.into()).build().unwrap();
        let responder_handler = ReverseTunnelResponderHandler {
            registration_crypto: header_crypto.clone(),
            stream_runtime: runtime.stream.clone(),
            udp_runtime: runtime.udp.clone(),
        };
        let responder_addr = match transport {
            ReverseTunnelTransport::Tcp => {
                let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
                let addr = listener.local_addr().unwrap();
                let server = TcpReverseTunnelResponder {
                    listener,
                    handler: responder_handler,
                };
                scope.spawn_required("tcp responder server", async move {
                    let (_tx, rx) = loading::replace_conn_handler_channel();
                    server.serve(rx).await.unwrap();
                });
                format!("tcp://{addr}").parse().unwrap()
            }
            ReverseTunnelTransport::Rtp => {
                let server = rtp_mux::RtpMuxServer::bind("127.0.0.1:0", false)
                    .await
                    .unwrap();
                let addr = server.listener().local_addr();
                let server = RtpReverseTunnelResponder {
                    server,
                    handler: responder_handler,
                    session_spawner,
                };
                scope.spawn_required("rtp responder server", async move {
                    let (_tx, rx) = loading::replace_conn_handler_channel();
                    server.serve(rx).await.unwrap();
                });
                format!("rtpmux://{addr}").parse().unwrap()
            }
        };
        let config = initiator_config(
            "private-udp",
            responder_addr,
            ConfigBuilder(header_key.into()),
            Some(ConfigBuilder(payload_key.into())),
        );
        let initiator = ReverseTunnelInitiator {
            handler: ReverseTunnelInitiatorBuilder::new(config, runtime.clone())
                .unwrap()
                .handler()
                .unwrap(),
        };
        scope.spawn_required("initiator", async move {
            let (_tx, rx) = loading::replace_conn_handler_channel();
            initiator.serve(rx).await.unwrap();
        });
        let origin = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let origin_addr = origin.local_addr().unwrap();
        scope.spawn(async move {
            let mut packet = [0; 64];
            for (request, response) in
                [(b"ping".as_slice(), b"pong".as_slice()), (b"next", b"done")]
            {
                let (n, peer) = origin.recv_from(&mut packet).await.unwrap();
                assert_eq!(&packet[..n], request);
                origin.send_to(response, peer).await.unwrap();
            }
        });
        let reverse_addr: RouteAddr = format!("{}://private-udp", transport.protocol())
            .parse()
            .unwrap();
        let mut chain = Vec::new();
        if behind_regular_udp_hop {
            let first_header_crypto = tokio_chacha20::config::Config::new([9; 32].into());
            let first_payload_crypto = tokio_chacha20::config::Config::new([10; 32].into());
            let server = UdpProxyConnHandler::new(
                first_header_crypto.clone(),
                Some(first_payload_crypto.clone()),
                runtime.udp.clone(),
                true,
            )
            .build("127.0.0.1:0")
            .await
            .unwrap();
            let server_addr = server.listener().local_addr().unwrap();
            scope.spawn_required("regular UDP proxy server", async move {
                let (_tx, rx) = loading::replace_conn_handler_channel();
                server.serve(rx).await.unwrap();
            });
            chain.push(ConnConfig {
                address: RouteAddr::udp(server_addr.into()),
                header_crypto: first_header_crypto,
                payload_crypto: Some(first_payload_crypto),
            });
        }
        chain.push(ConnConfig {
            address: reverse_addr,
            header_crypto,
            payload_crypto: Some(payload_crypto),
        });
        let chain: Arc<ConnChain> = chain.into();
        scope
            .run(async {
                let client = tokio::time::timeout(
                    Duration::from_secs(10),
                    UdpProxyClient::establish(chain, origin_addr.into(), &runtime.udp),
                )
                .await
                .expect("UDP reverse tunnel did not register")
                .unwrap();
                let (mut read, mut write) = client.into_split();
                tokio::time::timeout(Duration::from_secs(10), async {
                    write.send(b"ping").await.unwrap();
                    let mut response = [0; 4];
                    let n = read.recv(&mut response).await.unwrap();
                    assert_eq!(n, response.len());
                    assert_eq!(&response, b"pong");
                    write.send(b"next").await.unwrap();
                    let n = read.recv(&mut response).await.unwrap();
                    assert_eq!(n, response.len());
                    assert_eq!(&response, b"done");
                })
                .await
                .expect("encrypted UDP reverse proxy flow stalled");
            })
            .await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn tcp_reverse_tunnel_carries_udp_with_payload_encryption() {
        verify_reverse_udp_proxy_hop(ReverseTunnelTransport::Tcp, false).await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn rtp_reverse_tunnel_is_a_later_udp_hop_with_per_hop_payload_encryption() {
        verify_reverse_udp_proxy_hop(ReverseTunnelTransport::Rtp, true).await;
    }

    async fn verify_reverse_proxy_hop(transport: ReverseTunnelTransport) {
        let mut scope = TestScope::new();
        let (runtime, session_spawner) = test_runtime(&mut scope).await;
        let crypto = tokio_chacha20::config::Config::new([7; 32].into());
        let responder_handler = ReverseTunnelResponderHandler {
            registration_crypto: crypto.clone(),
            stream_runtime: runtime.stream.clone(),
            udp_runtime: runtime.udp.clone(),
        };
        let responder_addr = match transport {
            ReverseTunnelTransport::Tcp => {
                let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
                let addr = listener.local_addr().unwrap();
                let server = TcpReverseTunnelResponder {
                    listener,
                    handler: responder_handler,
                };
                scope.spawn_required("tcp responder server", async move {
                    let (_tx, rx) = loading::replace_conn_handler_channel();
                    server.serve(rx).await.unwrap();
                });
                format!("tcp://{addr}").parse().unwrap()
            }
            ReverseTunnelTransport::Rtp => {
                let server = rtp_mux::RtpMuxServer::bind("127.0.0.1:0", false)
                    .await
                    .unwrap();
                let addr = server.listener().local_addr();
                let server = RtpReverseTunnelResponder {
                    server,
                    handler: responder_handler,
                    session_spawner,
                };
                scope.spawn_required("rtp responder server", async move {
                    let (_tx, rx) = loading::replace_conn_handler_channel();
                    server.serve(rx).await.unwrap();
                });
                format!("rtpmux://{addr}").parse().unwrap()
            }
        };
        let initiator = ReverseTunnelInitiator {
            handler: initiator_handler(
                "private-a",
                responder_addr,
                transport,
                crypto.clone(),
                runtime.clone(),
            ),
        };
        scope.spawn_required("initiator", async move {
            let (_tx, rx) = loading::replace_conn_handler_channel();
            initiator.serve(rx).await.unwrap();
        });
        let origin_addr = origin(&mut scope).await;
        let reverse_addr: RouteAddr = format!("{}://private-a", transport.protocol())
            .parse()
            .unwrap();
        let chain = [ConnConfig {
            address: reverse_addr,
            header_crypto: crypto,
            payload_crypto: None,
        }];
        let destination: RouteAddr = format!("tcp://{origin_addr}").parse().unwrap();
        scope
            .run(async {
                let mut stream = tokio::time::timeout(
                    Duration::from_secs(10),
                    stream::establish(&chain, destination, &runtime.stream),
                )
                .await
                .expect("reverse tunnel did not register")
                .unwrap()
                .stream;
                tokio::time::timeout(Duration::from_secs(10), async {
                    stream.write_all(b"ping").await.unwrap();
                    let mut response = [0; 4];
                    stream.read_exact(&mut response).await.unwrap();
                    assert_eq!(&response, b"pong");
                })
                .await
                .expect("reverse proxy stream stalled");
            })
            .await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn tcp_reverse_tunnel_is_a_named_proxy_hop() {
        verify_reverse_proxy_hop(ReverseTunnelTransport::Tcp).await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn rtp_reverse_tunnel_is_a_named_proxy_hop() {
        verify_reverse_proxy_hop(ReverseTunnelTransport::Rtp).await;
    }
}
