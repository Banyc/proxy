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
use tokio::{net::ToSocketAddrs, task::JoinSet};
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
pub struct RtpMuxServer<ConnHandler> {
    listener: rtp::udp::Listener,
    mux: JoinSet<MuxError>,
    conn_handler: ConnHandler,
    fec: bool,
}
impl<ConnHandler> RtpMuxServer<ConnHandler> {
    pub fn new(listener: rtp::udp::Listener, conn_handler: ConnHandler, fec: bool) -> Self {
        Self {
            listener,
            mux: JoinSet::new(),
            conn_handler,
            fec,
        }
    }
    pub fn listener(&self) -> &rtp::udp::Listener {
        &self.listener
    }
}
impl<ConnHandler> loading::Serve for RtpMuxServer<ConnHandler>
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
impl<ConnHandler> RtpMuxServer<ConnHandler>
where
    ConnHandler: StreamServerHandleConn + Send + Sync + 'static,
{
    #[instrument(skip(self))]
    async fn serve_(
        mut self,
        mut set_conn_handler_rx: tokio::sync::mpsc::Receiver<ConnHandler>,
    ) -> Result<(), ServeError> {
        let addr = self.listener.local_addr();
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
                res = self.listener.accept_without_handshake(self.fec) => {
                    let stream = match res {
                        Ok(res) => res,
                        Err(e) => {
                            warn!(?e, ?addr, "RTP accept error");
                            continue;
                        }
                    };
                    counter!("stream.rtp_mux.rtp.accepts").increment(1);
                    let addr = SocketAddrPair {
                        local_addr: self.listener.local_addr(),
                        peer_addr: stream.peer_addr,
                    };
                    let r = stream.read.into_async_read();
                    let w = stream.write.into_async_write();
                    let (_, accepter) = spawn_mux_no_reconnection(r, w, server_mux_config(), &mut self.mux);
                    let conn_handler = Arc::clone(&conn_handler);
                    accepting.spawn(async move {
                        run_mux_accepter(accepter, addr, |stream| {
                            counter!("stream.rtp_mux.mux.accepts").increment(1);
                            let conn_handler = Arc::clone(&conn_handler);
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
pub struct RtpMuxConnector {
    connect_request_tx: ConnectRequestTx,
    _connector: JoinSet<()>,
}
impl RtpMuxConnector {
    pub fn new(config: Arc<RwLock<ConnectorConfig>>, fec: bool) -> Self {
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
                    let connected =
                        rtp::udp::connect_without_handshake(bind, addr, None, fec).await?;
                    let addr = SocketAddrPair {
                        local_addr: connected.local_addr,
                        peer_addr: connected.peer_addr,
                    };
                    counter!("stream.rtp_mux.rtp.connects").increment(1);
                    let r = connected.read.into_async_read();
                    let w = connected.write.into_async_write();
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
impl StreamConnect for RtpMuxConnector {
    async fn connect(&self, addr: SocketAddr) -> io::Result<Box<dyn AsConn>> {
        let ((r, w), addr) = self.connect_request_tx.send(addr).await?;
        counter!("stream.rtp_mux.mux.connects").increment(1);
        let stream = PollIo::new(PollRead::new(r), PollWrite::new(w));
        Ok(Box::new(IoMuxStream::new(stream, addr)))
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RtpMuxListenerConfig {
    pub listen_addr: Arc<str>,
    pub fec: bool,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RtpMuxProxyServerConfig {
    #[serde(flatten)]
    pub listener: RtpMuxListenerConfig,
    #[serde(flatten)]
    pub inner: StreamProxyConnHandlerConfig,
}
impl RtpMuxProxyServerConfig {
    pub fn into_builder(self, stream_context: StreamContext) -> RtpMuxProxyServerBuilder {
        let listen_addr = Arc::clone(&self.listener.listen_addr);
        let inner = self.inner.into_builder(stream_context, listen_addr);
        RtpMuxProxyServerBuilder {
            listener: self.listener,
            inner,
        }
    }
}

#[derive(Debug, Clone)]
pub struct RtpMuxProxyServerBuilder {
    pub listener: RtpMuxListenerConfig,
    pub inner: StreamProxyConnHandlerBuilder,
}
impl loading::Build for RtpMuxProxyServerBuilder {
    type ConnHandler = StreamProxyConnHandler;
    type Server = RtpMuxServer<Self::ConnHandler>;
    type Err = RtpMuxProxyServerBuildError;

    async fn build_server(self) -> Result<Self::Server, Self::Err> {
        let listener = self.listener.clone();
        let stream_proxy = self.build_conn_handler()?;
        build_rtp_mux_proxy_server(listener.listen_addr.as_ref(), stream_proxy, listener.fec)
            .await
            .map_err(|e| e.into())
    }

    fn build_conn_handler(self) -> Result<Self::ConnHandler, Self::Err> {
        self.inner.build().map_err(|e| e.into())
    }

    fn key(&self) -> &Arc<str> {
        &self.listener.listen_addr
    }
}
#[derive(Debug, Error)]
pub enum RtpMuxProxyServerBuildError {
    #[error("{0}")]
    Hook(#[from] StreamProxyServerBuildError),
    #[error("{0}")]
    Server(#[from] ListenerBindError),
}
pub async fn build_rtp_mux_proxy_server(
    listen_addr: impl ToSocketAddrs,
    stream_proxy: StreamProxyConnHandler,
    fec: bool,
) -> Result<RtpMuxServer<StreamProxyConnHandler>, ListenerBindError> {
    let listener = rtp::udp::Listener::bind(listen_addr)
        .await
        .map_err(ListenerBindError)?;
    let server = RtpMuxServer::new(listener, stream_proxy, fec);
    Ok(server)
}
