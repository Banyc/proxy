use std::{
    io,
    net::SocketAddr,
    sync::{Arc, RwLock},
};

use async_trait::async_trait;
use common::{
    addr::any_addr,
    connect::{ConnectorConfig, ConnectorResetSignal},
    error::AnyResult,
    loading::{self, ReloadableHandler},
    proto::{
        conn_handler::{
            ListenerBindError,
            stream::StreamProxyServerBuildError,
            stream::{StreamProxyConnHandlerBuilder, StreamProxyConnHandlerConfig},
        },
        connect::{
            stream::StreamConnect,
            udp::{UdpConnection, UdpMuxDialer},
        },
        context::Runtime,
    },
    session::{SessionSpawner, log_rejection},
    stream::ConnParts,
};
use metrics::counter;
use mux::{MuxError, spawn_mux_no_reconnection};
use serde::Deserialize;
use thiserror::Error;
use tokio::{
    net::{TcpListener, TcpSocket, TcpStream, ToSocketAddrs},
    task::JoinSet,
};
use tracing::{instrument, warn};

use crate::stream::streams::mux::{
    AddressedMuxStream, ConnectRequestTx, ConnectorDriverError, MuxConnectorDriver, MuxFlowKind,
    MuxProxyConnHandler, MuxProxyHandler, MuxProxyUdpBuildError, SocketAddrPair,
    build_udp_proxy_handler, connect_request_channel, dispatch_mux_flow, run_mux_accepter,
    run_mux_connector, server_mux_config, write_flow_kind,
};

struct MuxState {
    mux: JoinSet<MuxError>,
    accepting: JoinSet<()>,
}

#[derive(Debug)]
pub struct TcpMuxServer<ConnHandler> {
    listener: TcpListener,
    mux: JoinSet<MuxError>,
    reloadable: ReloadableHandler<ConnHandler>,
    session_spawner: SessionSpawner,
}
impl<ConnHandler> TcpMuxServer<ConnHandler> {
    pub fn new(
        listener: TcpListener,
        conn_handler: ConnHandler,
        session_spawner: SessionSpawner,
    ) -> Self {
        Self::with_reloadable(
            listener,
            ReloadableHandler::new(conn_handler),
            session_spawner,
        )
    }
    /// Construct with a caller-owned [`ReloadableHandler`], so a test can
    /// subscribe to its generation watch for deterministic reload
    /// acknowledgement.
    pub fn with_reloadable(
        listener: TcpListener,
        reloadable: ReloadableHandler<ConnHandler>,
        session_spawner: SessionSpawner,
    ) -> Self {
        Self {
            listener,
            mux: JoinSet::new(),
            reloadable,
            session_spawner,
        }
    }
    pub fn reloadable(&self) -> &ReloadableHandler<ConnHandler> {
        &self.reloadable
    }
    pub fn listener(&self) -> &TcpListener {
        &self.listener
    }
}
impl<ConnHandler> loading::Serve for TcpMuxServer<ConnHandler>
where
    ConnHandler: MuxProxyConnHandler,
{
    type ConnHandler = ConnHandler;

    async fn serve(
        self,
        set_conn_handler_rx: loading::ReplaceConnHandlerRx<ConnHandler>,
    ) -> AnyResult {
        self.serve_(set_conn_handler_rx).await.map_err(|e| e.into())
    }
}
impl<ConnHandler> TcpMuxServer<ConnHandler>
where
    ConnHandler: MuxProxyConnHandler,
{
    #[instrument(skip_all)]
    async fn serve_(
        self,
        set_conn_handler_rx: loading::ReplaceConnHandlerRx<ConnHandler>,
    ) -> Result<(), ServeLoopError> {
        let addr = self
            .listener
            .local_addr()
            .map_err(ServeLoopError::LocalAddr)?;
        let mut state = MuxState {
            mux: self.mux,
            accepting: JoinSet::new(),
        };
        let listener = &self.listener;
        let session_spawner = self.session_spawner.clone();
        // The current handler, shared with every established TCP mux session.
        // A reload replaces it here (via `on_handler_replaced`) and bumps the
        // generation watch; each accepted mux substream clones whatever is
        // current, so reloads reach substreams opened on sessions that
        // predate the reload instead of being pinned to the TCP-accept-time
        // handler.
        let reloadable = self.reloadable.clone();
        common::serve_loop::serve_loop(
            addr,
            reloadable.current(),
            set_conn_handler_rx,
            {
                let reloadable = reloadable.clone();
                move |new_handler: Arc<ConnHandler>| {
                    reloadable.replace(new_handler);
                }
            },
            || listener.accept(),
            |state: &mut MuxState,
             (stream, _): (TcpStream, SocketAddr),
             _conn_handler: Arc<ConnHandler>| {
                let addr = match socket_addr_pair(&stream) {
                    Ok(addr) => addr,
                    Err(_) => return Box::pin(async {}),
                };
                let (r, w) = stream.into_split();
                let (_, accepter) =
                    spawn_mux_no_reconnection(r, w, server_mux_config(), &mut state.mux);
                let session_spawner = session_spawner.clone();
                let reloadable = reloadable.clone();
                state.accepting.spawn(async move {
                    run_mux_accepter(accepter, addr, |(reader, writer)| {
                        counter!("stream.tcp_mux.mux.accepts").increment(1);
                        let conn_handler = reloadable.current();
                        let session_spawner = session_spawner.clone();
                        Box::pin(async move {
                            if let Err(error) = session_spawner
                                .spawn(async move {
                                    let stream =
                                        tokio_chacha20::stream::DuplexStream::new(reader, writer);
                                    dispatch_mux_flow(
                                        stream,
                                        addr,
                                        conn_handler,
                                        AddressedMuxStream::new,
                                        "stream.tcp_mux.udp.flows",
                                    )
                                    .await;
                                    Ok(())
                                })
                                .await
                            {
                                log_rejection("tcp_mux", error);
                            }
                        })
                    })
                    .await;
                });
                Box::pin(async {})
            },
            &mut state,
            |state: &mut MuxState| {
                Box::pin(async move {
                    tokio::select! {
                        Some(res) = state.mux.join_next() => {
                            let error = res.unwrap();
                            warn!(?error, ?addr, "MUX error");
                        }
                        Some(result) = state.accepting.join_next() => {
                            result.unwrap();
                        }
                        _ = std::future::pending::<()>() => {}
                    }
                })
            },
            common::serve_loop::ServeLoopConfig {
                label: "tcp_mux",
                counter_name: Some("stream.tcp_mux.tcp.accepts"),
                counts_dispatch_errors: false,
            },
        )
        .await
    }
}
pub use common::serve_loop::ServeLoopError;

/// Injectable query for a stream's local/peer address pair.
///
/// [`TcpStream`] answers from the live socket. A fake can stand in for tests
/// that must deterministically exercise the reset path: dropping a listener
/// does not guarantee that an established connection is immediately reset,
/// and the socket's address getters may keep answering afterwards.
trait SocketAddrQuery {
    fn socket_addr_pair(&self) -> io::Result<SocketAddrPair>;
}
impl SocketAddrQuery for TcpStream {
    fn socket_addr_pair(&self) -> io::Result<SocketAddrPair> {
        Ok(SocketAddrPair {
            local_addr: self.local_addr()?,
            peer_addr: self.peer_addr()?,
        })
    }
}

fn socket_addr_pair(query: &impl SocketAddrQuery) -> io::Result<SocketAddrPair> {
    query.socket_addr_pair()
}

#[derive(Debug)]
pub struct TcpMuxConnector {
    connect_request_tx: ConnectRequestTx,
}
impl TcpMuxConnector {
    pub fn new(
        config: Arc<RwLock<ConnectorConfig>>,
        reset: ConnectorResetSignal,
    ) -> (Self, MuxConnectorDriver) {
        let (connect_request_tx, connect_request_rx) = connect_request_channel();
        let driver = async move {
            run_mux_connector(reset, connect_request_rx, move |addr| {
                let config = config.clone();
                async move {
                    let bind = config
                        .read()
                        .unwrap()
                        .bind
                        .get_matched(&addr.ip())
                        .map(|ip| SocketAddr::new(ip, 0))
                        .unwrap_or_else(|| any_addr(&addr.ip()));
                    let socket = match addr.ip() {
                        std::net::IpAddr::V4(_) => TcpSocket::new_v4()?,
                        std::net::IpAddr::V6(_) => TcpSocket::new_v6()?,
                    };
                    socket.bind(bind)?;
                    let stream = socket.connect(addr).await?;
                    let addr = socket_addr_pair(&stream)?;
                    counter!("stream.tcp_mux.tcp.connects").increment(1);
                    let (r, w) = stream.into_split();
                    Ok(((r, w), addr))
                }
            })
            .await;
            ConnectorDriverError::ConnectorExited
        };
        (Self { connect_request_tx }, MuxConnectorDriver::new(driver))
    }
}
#[async_trait]
impl UdpMuxDialer for TcpMuxConnector {
    /// Open a UDP datagram flow to a remote mux proxy.
    ///
    /// Opens a fresh mux stream, writes the UDP flow-kind byte, and frames
    /// datagrams exactly like reverse tunneling: `[kind=1]` followed by
    /// `udp_mux` length-prefixed datagrams.
    async fn dial_udp(&self, addr: SocketAddr) -> io::Result<UdpConnection> {
        let ((reader, mut writer), addr) = self.connect_request_tx.send(addr).await?;
        counter!("stream.tcp_mux.mux.connects").increment(1);
        write_flow_kind(&mut writer, MuxFlowKind::Udp).await?;
        Ok(UdpConnection::mux_io(
            reader,
            writer,
            addr.local_addr,
            addr.peer_addr,
        ))
    }
}
#[async_trait]
impl StreamConnect for TcpMuxConnector {
    async fn connect(&self, addr: SocketAddr) -> io::Result<Box<dyn ConnParts>> {
        let ((reader, mut writer), addr) = self.connect_request_tx.send(addr).await?;
        counter!("stream.tcp_mux.mux.connects").increment(1);
        write_flow_kind(&mut writer, MuxFlowKind::Stream).await?;
        let stream = tokio_chacha20::stream::DuplexStream::new(reader, writer);
        Ok(Box::new(AddressedMuxStream::new(stream, addr)))
    }
}
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TcpMuxProxyServerConfig {
    pub listen_addr: Arc<str>,
    #[serde(flatten)]
    pub inner: StreamProxyConnHandlerConfig,
}
impl TcpMuxProxyServerConfig {
    pub fn into_builder(self, runtime: Runtime) -> TcpMuxProxyServerBuilder {
        let listen_addr = Arc::clone(&self.listen_addr);
        let inner = self.inner.into_builder(runtime.stream, listen_addr);
        TcpMuxProxyServerBuilder {
            listen_addr: self.listen_addr,
            inner,
            udp_context: runtime.udp,
        }
    }
}

#[derive(Debug, Clone)]
pub struct TcpMuxProxyServerBuilder {
    pub listen_addr: Arc<str>,
    pub inner: StreamProxyConnHandlerBuilder,
    pub udp_context: common::proto::context::UdpRuntime,
}
impl loading::Build for TcpMuxProxyServerBuilder {
    type ConnHandler = MuxProxyHandler;
    type Server = TcpMuxServer<Self::ConnHandler>;
    type Err = TcpMuxProxyServerBuildError;

    async fn build_server(self) -> Result<Self::Server, Self::Err> {
        let listen_addr = self.listen_addr.clone();
        let session_spawner = self.inner.stream_context.session_spawner.clone();
        let handler = self.build_conn_handler()?;
        build_tcp_mux_proxy_server(listen_addr.as_ref(), handler, session_spawner)
            .await
            .map_err(|e| e.into())
    }

    fn build_conn_handler(self) -> Result<Self::ConnHandler, Self::Err> {
        let stream = self
            .inner
            .clone()
            .build()
            .map_err(TcpMuxProxyServerBuildError::Hook)?;
        let udp = build_udp_proxy_handler(
            self.inner.header_key,
            self.inner.payload_key,
            self.udp_context,
            self.inner.allow_loopback,
        )
        .map_err(TcpMuxProxyServerBuildError::Udp)?;
        Ok(MuxProxyHandler { stream, udp })
    }

    fn key(&self) -> &Arc<str> {
        &self.listen_addr
    }
}
#[derive(Debug, Error)]
pub enum TcpMuxProxyServerBuildError {
    #[error("{0}")]
    Hook(#[from] StreamProxyServerBuildError),
    #[error("{0}")]
    Udp(#[from] MuxProxyUdpBuildError),
    #[error("{0}")]
    Server(#[from] ListenerBindError),
}
pub async fn build_tcp_mux_proxy_server(
    listen_addr: impl ToSocketAddrs,
    handler: MuxProxyHandler,
    session_spawner: SessionSpawner,
) -> Result<TcpMuxServer<MuxProxyHandler>, ListenerBindError> {
    let listener = TcpListener::bind(listen_addr)
        .await
        .map_err(ListenerBindError)?;
    let server = TcpMuxServer::new(listener, handler, session_spawner);
    Ok(server)
}
#[cfg(test)]
mod tests {
    use super::*;

    /// Always fails, as a connection reset does at the address-query layer.
    struct ResetAddressQuery;
    impl SocketAddrQuery for ResetAddressQuery {
        fn socket_addr_pair(&self) -> io::Result<SocketAddrPair> {
            Err(io::Error::new(
                io::ErrorKind::ConnectionReset,
                "connection reset",
            ))
        }
    }

    #[test]
    fn a_reset_connection_is_an_error_not_a_panic() {
        let error = socket_addr_pair(&ResetAddressQuery).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::ConnectionReset);
    }
}
