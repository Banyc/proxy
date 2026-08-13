//! The responder side of a reverse tunnel: validates the initiator's
//! registration, registers named stream/UDP connectors on the incoming
//! tunnel, and serves the TCP/RTP mux listeners that accept sessions.

use std::{io, sync::Arc, time::Instant};

use ae::anti_replay::ValidatorRef;
use async_trait::async_trait;
use common::{
    error::AnyResult,
    header::codec::{timed_read_header_async, timed_write_header_async},
    loading,
    proto::{
        addr::{
            REVERSE_TUNNEL_RTP_PROTOCOL, REVERSE_TUNNEL_TCP_PROTOCOL, ReverseTunnelTransport,
            RouteAddr, validate_reverse_tunnel_name,
        },
        connect::{
            stream::NamedStreamConnect,
            udp::{NamedUdpConnect, UdpConnection},
        },
        context::{Runtime, StreamRuntime, UdpRuntime},
    },
    session::log_rejection,
    stream::{ConnParts, HasIoAddr},
};
use metrics::{counter, gauge};
use mux::{LaneClass, spawn_mux_no_reconnection};
use tokio::{net::TcpListener, task::JoinSet};
use tracing::{info, warn};

use crate::stream::streams::{
    mux::{AddressedMuxStream, MuxFlowKind, SocketAddrPair, server_mux_config, write_flow_kind},
    tcp::proxy_server::AddressedTcpStream,
};

use super::{
    loading::BuildError,
    wire::{
        REGISTER_VERSION, RegisterError, RegisterRequest, RegisterResponse,
        ReverseTunnelSessionError, mux_result,
    },
};

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
            "peer={},uptime={:?}",
            self.addr.peer_addr,
            self.connected_at.elapsed()
        ))
    }
}

#[derive(Debug)]
pub struct ReverseTunnelResponderHandler {
    pub(crate) registration_crypto: tokio_chacha20::config::Config,
    pub(crate) stream_runtime: StreamRuntime,
    pub(crate) udp_runtime: UdpRuntime,
}
impl loading::HandleConn for ReverseTunnelResponderHandler {}

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
    pub(crate) listener: TcpListener,
    pub(crate) handler: ReverseTunnelResponderHandler,
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
    pub(crate) server: rtp_mux::RtpMuxServer,
    pub(crate) handler: ReverseTunnelResponderHandler,
    pub(crate) session_spawner: common::session::SessionSpawner,
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
        let (shutdown, shutdown_rx) = tokio::sync::watch::channel(false);
        let serving = self.server.serve_sessions_with_shutdown(
            rtp_session_spawner,
            move |session| {
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
            },
            shutdown_rx,
        );
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
                        Err(()) => {
                            // The responder was removed: signal shutdown and
                            // keep polling the serving future until every
                            // nested rtp_mux scope has been reaped.
                            shutdown.send_replace(true);
                            return serving.await.map_err(Into::into);
                        }
                    }
                }
            }
        }
    }
}

#[derive(Debug)]
pub struct TcpReverseTunnelResponderBuilder {
    pub(crate) key: Arc<str>,
    pub(crate) listen_addr: RouteAddr,
    pub(crate) header_key: tokio_chacha20::config::ConfigBuilder,
    pub(crate) runtime: Runtime,
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
    pub(crate) key: Arc<str>,
    pub(crate) listen_addr: RouteAddr,
    pub(crate) header_key: tokio_chacha20::config::ConfigBuilder,
    pub(crate) runtime: Runtime,
    pub(crate) fec: bool,
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
