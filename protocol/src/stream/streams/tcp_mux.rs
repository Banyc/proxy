use std::{
    io,
    net::SocketAddr,
    sync::{Arc, RwLock},
};

use async_trait::async_trait;
use metrics::counter;
use mux::{
    MuxError,
    async_async_io::{PollIo, read::PollRead, write::PollWrite},
    spawn_mux_no_reconnection,
};
use serde::Deserialize;
use thiserror::Error;
use tokio::{
    net::{TcpListener, TcpSocket, ToSocketAddrs},
    task::JoinSet,
};
use tracing::{info, instrument, trace, warn};

use common::{
    addr::any_addr,
    connect::ConnectorConfig,
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
        context::StreamContext,
    },
    stream::{AsConn, StreamServerHandleConn},
};

use crate::stream::streams::mux::{run_mux_accepter, server_mux_config};

use super::mux::{
    ConnectRequestTx, IoMuxStream, SocketAddrPair, connect_request_channel, run_mux_connector,
};

#[derive(Debug)]
pub struct TcpMuxServer<ConnHandler> {
    listener: TcpListener,
    mux: JoinSet<MuxError>,
    conn_handler: ConnHandler,
}
impl<ConnHandler> TcpMuxServer<ConnHandler> {
    pub fn new(listener: TcpListener, conn_handler: ConnHandler) -> Self {
        Self {
            listener,
            mux: JoinSet::new(),
            conn_handler,
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
        set_conn_handler_rx: tokio::sync::mpsc::Receiver<Self::ConnHandler>,
    ) -> AnyResult {
        self.serve_(set_conn_handler_rx).await.map_err(|e| e.into())
    }
}
impl<ConnHandler> TcpMuxServer<ConnHandler>
where
    ConnHandler: StreamServerHandleConn + Send + Sync + 'static,
{
    #[instrument(skip(self))]
    async fn serve_(
        mut self,
        mut set_conn_handler_rx: tokio::sync::mpsc::Receiver<ConnHandler>,
    ) -> Result<(), ServeError> {
        let addr = self.listener.local_addr().map_err(ServeError::LocalAddr)?;
        info!(?addr, "Listening");
        // Arc conn_handler
        let mut conn_handler = Arc::new(self.conn_handler);
        let mut accepting = JoinSet::new();
        loop {
            trace!("Waiting for connection");
            tokio::select! {
                Some(res) = self.mux.join_next() => {
                    let e = res.unwrap();
                    warn!(?e, ?addr, "MUX error");
                }
                res = self.listener.accept() => {
                    // let (stream, _) = res.map_err(|e| ServeError::Accept { source: e, addr })?;
                    let (stream, _) = match res {
                        Ok(res) => res,
                        Err(e) => {
                            warn!(?e, ?addr, "TCP accept error");
                            continue;
                        }
                    };
                    counter!("stream.tcp_mux.tcp.accepts").increment(1);
                    let addr = || -> io::Result<SocketAddrPair> {
                        Ok(SocketAddrPair {
                            local_addr: stream.local_addr()?,
                            peer_addr: stream.peer_addr()?,
                        })
                    };
                    let addr = match addr() {
                        Ok(addr) => addr,
                        Err(_) => {
                            continue;
                        }
                    };
                    let (r, w) = stream.into_split();
                    let (_, accepter) = spawn_mux_no_reconnection(r, w, server_mux_config(), &mut self.mux);
                    let conn_handler = Arc::clone(&conn_handler);
                    accepting.spawn(async move {
                        run_mux_accepter(accepter, addr, |stream| {
                            counter!("stream.tcp_mux.mux.accepts").increment(1);
                            let conn_handler = Arc::clone(&conn_handler);
                            // Arc conn_handler
                            tokio::spawn(async move {
                                conn_handler.handle_stream(stream).await;
                            });
                        }).await;
                    });
                }
                res = set_conn_handler_rx.recv() => {
                    let new_conn_handler = match res {
                        Some(new_conn_handler) => new_conn_handler,
                        None => break,
                    };
                    info!(?addr, "Connection handler set");
                    conn_handler = Arc::new(new_conn_handler);
                }
            }
        }
        Ok(())
    }
}
#[derive(Debug, Error)]
pub enum ServeError {
    #[error("Failed to get local address: {0}")]
    LocalAddr(#[source] io::Error),
    #[error("Failed to accept connection: {source}, {addr}")]
    Accept {
        #[source]
        source: io::Error,
        addr: SocketAddr,
    },
}

#[derive(Debug)]
pub struct TcpMuxConnector {
    connect_request_tx: ConnectRequestTx,
    _connector: JoinSet<()>,
}
impl TcpMuxConnector {
    pub fn new(config: Arc<RwLock<ConnectorConfig>>) -> Self {
        let (connect_request_tx, connect_request_rx) = connect_request_channel();
        let mut connector = JoinSet::new();
        connector.spawn(async move {
            run_mux_connector(connect_request_rx, move |addr| {
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
                    let addr = SocketAddrPair {
                        local_addr: stream.local_addr().unwrap(),
                        peer_addr: stream.peer_addr().unwrap(),
                    };
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
    async fn connect(&self, addr: SocketAddr) -> io::Result<Box<dyn AsConn>> {
        let ((r, w), addr) = self.connect_request_tx.send(addr).await?;
        counter!("stream.tcp_mux.mux.connects").increment(1);
        let stream = PollIo::new(PollRead::new(r), PollWrite::new(w));
        Ok(Box::new(IoMuxStream::new(stream, addr)))
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
    pub fn into_builder(self, stream_context: StreamContext) -> TcpMuxProxyServerBuilder {
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
        let stream_proxy = self.build_conn_handler()?;
        build_tcp_mux_proxy_server(listen_addr.as_ref(), stream_proxy)
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
) -> Result<TcpMuxServer<StreamProxyConnHandler>, ListenerBindError> {
    let listener = TcpListener::bind(listen_addr)
        .await
        .map_err(ListenerBindError)?;
    let server = TcpMuxServer::new(listener, stream_proxy);
    Ok(server)
}
