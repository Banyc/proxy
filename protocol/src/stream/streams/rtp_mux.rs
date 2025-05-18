use std::{
    io,
    net::SocketAddr,
    sync::{Arc, RwLock},
};

use metrics::counter;
use mux::{
    MuxError,
    async_async_io::{PollIo, read::PollRead, write::PollWrite},
    spawn_mux_no_reconnection,
};
use thiserror::Error;
use tokio::task::JoinSet;
use tracing::{info, instrument, trace, warn};

use common::{
    addr::any_addr,
    connect::ConnectorConfig,
    error::AnyResult,
    loading,
    stream::{StreamServerHandleConn, connect::StreamConnect},
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
impl StreamConnect for RtpMuxConnector {
    type Conn = IoMuxStream;
    async fn connect(&self, addr: SocketAddr) -> io::Result<Self::Conn> {
        let ((r, w), addr) = self.connect_request_tx.send(addr).await?;
        counter!("stream.rtp_mux.mux.connects").increment(1);
        let stream = PollIo::new(PollRead::new(r), PollWrite::new(w));
        Ok(IoMuxStream::new(stream, addr))
    }
}
