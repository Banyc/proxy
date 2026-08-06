use std::{
    io,
    net::SocketAddr,
    sync::{Arc, RwLock},
};

use async_trait::async_trait;
use metrics::counter;
use mux::{MuxError, spawn_mux_no_reconnection};
use serde::Deserialize;
use thiserror::Error;
use tokio::{
    net::{TcpListener, TcpSocket, TcpStream, ToSocketAddrs},
    task::JoinSet,
};
use tracing::{error, instrument, trace, warn};

use common::{
    addr::any_addr,
    connect::{ConnectorConfig, ConnectorResetSignal},
    error::AnyResult,
    loading,
    proto::{
        conn_handler::{
            ListenerBindError,
            stream::{
                StreamProxyConnHandler, StreamProxyConnHandlerBuilder,
                StreamProxyConnHandlerConfig, StreamProxyServerBuildError,
            },
        },
        connect::stream::StreamConnect,
        context::StreamRuntime,
    },
    session::{SessionSpawner, log_rejection},
    stream::{ConnParts, StreamServerHandleConn},
};

use crate::stream::streams::mux::{run_mux_accepter, server_mux_config};

use super::mux::{
    AddressedMuxStream, ConnectRequestTx, SocketAddrPair, connect_request_channel,
    run_mux_connector,
};

struct MuxState {
    mux: JoinSet<MuxError>,
    accepting: JoinSet<()>,
}

#[derive(Debug)]
pub struct TcpMuxServer<ConnHandler> {
    listener: TcpListener,
    mux: JoinSet<MuxError>,
    conn_handler: ConnHandler,
    session_spawner: SessionSpawner,
}
impl<ConnHandler> TcpMuxServer<ConnHandler> {
    pub fn new(
        listener: TcpListener,
        conn_handler: ConnHandler,
        session_spawner: SessionSpawner,
    ) -> Self {
        Self {
            listener,
            mux: JoinSet::new(),
            conn_handler,
            session_spawner,
        }
    }
    pub fn listener(&self) -> &TcpListener {
        &self.listener
    }
}
impl<ConnHandler> loading::Serve for TcpMuxServer<ConnHandler>
where
    ConnHandler: StreamServerHandleConn + Send + Sync + 'static,
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
    ConnHandler: StreamServerHandleConn + Send + Sync + 'static,
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
        common::serve_loop::serve_loop(
            "tcp_mux",
            Some("stream.tcp_mux.tcp.accepts"),
            false,
            addr,
            Arc::new(self.conn_handler),
            set_conn_handler_rx,
            |_| {},
            || listener.accept(),
            |state: &mut MuxState,
             (stream, _): (TcpStream, SocketAddr),
             conn_handler: Arc<ConnHandler>| {
                let addr = match socket_addr_pair(&stream) {
                    Ok(addr) => addr,
                    Err(_) => return Box::pin(async {}),
                };
                let (r, w) = stream.into_split();
                let (_, accepter) =
                    spawn_mux_no_reconnection(r, w, server_mux_config(), &mut state.mux);
                let session_spawner = session_spawner.clone();
                state.accepting.spawn(async move {
                    run_mux_accepter(accepter, addr, |stream| {
                        counter!("stream.tcp_mux.mux.accepts").increment(1);
                        let conn_handler = Arc::clone(&conn_handler);
                        let session_spawner = session_spawner.clone();
                        Box::pin(async move {
                            if let Err(error) = session_spawner
                                .spawn(async move {
                                    conn_handler.handle_stream(stream).await;
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
            |state: &mut MuxState| Box::pin(async move {
                tokio::select! {
                    Some(res) = state.mux.join_next() => {
                        match res {
                            Ok(e) => warn!(?e, ?addr, "MUX error"),
                            Err(error) if error.is_cancelled() => {
                                trace!(?error, "MUX task cancelled (normal shutdown/reset)");
                            }
                            Err(error) => error!(?error, ?addr, "MUX supervision task failed to join"),
                        }
                    }
                    Some(_) = state.accepting.join_next() => {}
                    _ = std::future::pending::<()>() => {}
                }
            }),
        )
        .await
    }
}
pub use common::serve_loop::ServeLoopError;

fn socket_addr_pair(stream: &tokio::net::TcpStream) -> io::Result<SocketAddrPair> {
    Ok(SocketAddrPair {
        local_addr: stream.local_addr()?,
        peer_addr: stream.peer_addr()?,
    })
}

#[derive(Debug)]
pub struct TcpMuxConnector {
    connect_request_tx: ConnectRequestTx,
    _connector: JoinSet<()>,
}
impl TcpMuxConnector {
    pub fn new(config: Arc<RwLock<ConnectorConfig>>, reset: ConnectorResetSignal) -> Self {
        let (connect_request_tx, connect_request_rx) = connect_request_channel();
        let mut connector = JoinSet::new();
        connector.spawn(async move {
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
        });
        Self {
            connect_request_tx,
            _connector: connector,
        }
    }
}
#[async_trait]
impl StreamConnect for TcpMuxConnector {
    async fn connect(&self, addr: SocketAddr) -> io::Result<Box<dyn ConnParts>> {
        let ((r, w), addr) = self.connect_request_tx.send(addr).await?;
        counter!("stream.tcp_mux.mux.connects").increment(1);
        let stream = tokio_chacha20::stream::DuplexStream::new(r, w);
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
    pub fn into_builder(self, stream_context: StreamRuntime) -> TcpMuxProxyServerBuilder {
        let listen_addr = Arc::clone(&self.listen_addr);
        let inner = self.inner.into_builder(stream_context, listen_addr);
        TcpMuxProxyServerBuilder {
            listen_addr: self.listen_addr,
            inner,
        }
    }
}

#[derive(Debug, Clone)]
pub struct TcpMuxProxyServerBuilder {
    pub listen_addr: Arc<str>,
    pub inner: StreamProxyConnHandlerBuilder,
}
impl loading::Build for TcpMuxProxyServerBuilder {
    type ConnHandler = StreamProxyConnHandler;
    type Server = TcpMuxServer<Self::ConnHandler>;
    type Err = TcpMuxProxyServerBuildError;

    async fn build_server(self) -> Result<Self::Server, Self::Err> {
        let listen_addr = self.listen_addr.clone();
        let session_spawner = self.inner.stream_context.session_spawner.clone();
        let stream_proxy = self.build_conn_handler()?;
        build_tcp_mux_proxy_server(listen_addr.as_ref(), stream_proxy, session_spawner)
            .await
            .map_err(|e| e.into())
    }

    fn build_conn_handler(self) -> Result<Self::ConnHandler, Self::Err> {
        self.inner.build().map_err(|e| e.into())
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
    Server(#[from] ListenerBindError),
}
pub async fn build_tcp_mux_proxy_server(
    listen_addr: impl ToSocketAddrs,
    stream_proxy: StreamProxyConnHandler,
    session_spawner: SessionSpawner,
) -> Result<TcpMuxServer<StreamProxyConnHandler>, ListenerBindError> {
    let listener = TcpListener::bind(listen_addr)
        .await
        .map_err(ListenerBindError)?;
    let server = TcpMuxServer::new(listener, stream_proxy, session_spawner);
    Ok(server)
}
#[cfg(test)]
mod tests {
    use super::*;
    use tokio::net::TcpStream;

    #[tokio::test]
    async fn a_reset_connection_is_an_error_not_a_panic() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let stream = TcpStream::connect(listener.local_addr().unwrap())
            .await
            .unwrap();
        drop(listener);
        let _ = stream.try_write(b"x");
        assert!(socket_addr_pair(&stream).is_err());
    }
}
