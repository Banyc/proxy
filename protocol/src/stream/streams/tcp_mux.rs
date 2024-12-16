use std::{
    collections::{hash_map, HashMap},
    io,
    net::SocketAddr,
    pin::Pin,
    sync::Arc,
    time::Duration,
};

use metrics::counter;
use mux::{
    async_async_io::{read::PollRead, write::PollWrite, PollIo},
    spawn_mux_no_reconnection, DeadControl, Initiation, MuxConfig, MuxError, StreamOpener,
    StreamReader, StreamWriter, TooManyOpenStreams,
};
use thiserror::Error;
use tokio::{
    io::{AsyncRead, AsyncWrite},
    net::{TcpListener, TcpStream},
    task::JoinSet,
};
use tracing::{info, instrument, trace, warn};

use common::{
    error::AnyResult,
    loading,
    stream::{connect::StreamConnect, IoAddr, IoStream, StreamServerHandleConn},
};

use crate::stream::connection::Connection;

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
                    let addr = SocketAddrPair {
                        local_addr: stream.local_addr().unwrap(),
                        peer_addr: stream.peer_addr().unwrap(),
                    };
                    let (r, w) = stream.into_split();
                    let (_, mut accepter) = spawn_mux_no_reconnection(r, w, server_mux_config(), &mut self.mux);
                    let conn_handler = Arc::clone(&conn_handler);
                    accepting.spawn(async move {
                        loop {
                            let (r, w) = match accepter.accept().await {
                                Ok(x) => x,
                                Err(DeadControl {}) => break,
                            };
                            counter!("stream.tcp_mux.mux.accepts").increment(1);
                            let stream = PollIo::new(PollRead::new(r), PollWrite::new(w));
                            let stream = IoTcpMuxStream::new(stream, addr);
                            let conn_handler = Arc::clone(&conn_handler);
                            // Arc conn_handler
                            tokio::spawn(async move {
                                conn_handler.handle_stream(stream).await;
                            });
                        }
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

fn server_mux_config() -> MuxConfig {
    MuxConfig {
        initiation: Initiation::Server,
        heartbeat_interval: Duration::from_secs(5),
    }
}
fn client_mux_config() -> MuxConfig {
    MuxConfig {
        initiation: Initiation::Client,
        heartbeat_interval: Duration::from_secs(5),
    }
}

#[derive(Debug)]
pub struct TcpMuxConnector {
    connect_request_tx: ConnectRequestTx,
    _connector: JoinSet<()>,
}
impl TcpMuxConnector {
    pub fn new() -> Self {
        let (connect_request_tx, connect_request_rx) = connect_request_channel();
        let mut connector = JoinSet::new();
        connector.spawn(async move {
            run_tcp_mux_connector(connect_request_rx).await;
        });
        Self {
            connect_request_tx,
            _connector: connector,
        }
    }
}
impl Default for TcpMuxConnector {
    fn default() -> Self {
        Self::new()
    }
}
impl StreamConnect for TcpMuxConnector {
    type Connection = Connection;

    async fn connect(&self, addr: SocketAddr) -> io::Result<Connection> {
        let ((r, w), addr) = self.connect_request_tx.send(addr).await?;
        counter!("stream.tcp_mux.mux.connects").increment(1);
        let stream = PollIo::new(PollRead::new(r), PollWrite::new(w));
        Ok(Connection::TcpMux(IoTcpMuxStream::new(stream, addr)))
    }
}

async fn run_tcp_mux_connector(mut connect_request_rx: ConnectRequestRx) {
    let mut openers: HashMap<SocketAddr, (StreamOpener, SocketAddrPair)> = HashMap::new();
    let mut mux_spawner: JoinSet<(SocketAddr, MuxError)> = JoinSet::new();
    loop {
        tokio::select! {
            Some(res) = mux_spawner.join_next() => {
                let (addr, e) = res.unwrap();
                warn!(?e, ?addr, "MUX error");
                openers.remove(&addr);
            }
            res = connect_request_rx.recv() => {
                let Some(msg) = res else {
                    break;
                };
                if let hash_map::Entry::Vacant(e) = openers.entry(msg.listen_addr) {
                    let (opener, addr) = match build_opener(msg.listen_addr, &mut mux_spawner).await {
                        Ok(x) => x,
                        Err(e) => {
                            let _ = msg.stream.send(Err(e));
                            continue;
                        }
                    };
                    e.insert((opener, addr));
                }
                let (opener, addr) = openers.get(&msg.listen_addr).unwrap();
                let stream = match opener.open().await {
                    Ok(x) => x,
                    Err(e) => {
                        let e = match e {
                            mux::StreamOpenError::DeadControl(DeadControl {}) => {
                                io::Error::new(io::ErrorKind::ConnectionReset, format!("dead control; {addr:?}"))
                            },
                            mux::StreamOpenError::TooManyOpenStreams(TooManyOpenStreams {}) => {
                                io::Error::new(io::ErrorKind::InvalidInput, format!("too many open streams; {addr:?}"))
                            },
                        };
                        let _ = msg.stream.send(Err(e));
                        openers.remove(&msg.listen_addr).unwrap();
                        continue;
                    }
                };
                let _ = msg.stream.send(Ok((stream, *addr)));
            }
        }
    }
}
async fn build_opener(
    listen_addr: SocketAddr,
    mux_spawner: &mut JoinSet<(SocketAddr, MuxError)>,
) -> io::Result<(StreamOpener, SocketAddrPair)> {
    let stream = TcpStream::connect(listen_addr).await?;
    let addr = SocketAddrPair {
        local_addr: stream.local_addr().unwrap(),
        peer_addr: stream.peer_addr().unwrap(),
    };
    counter!("stream.tcp_mux.tcp.connects").increment(1);
    let (r, w) = stream.into_split();
    let config = client_mux_config();
    let mut spawner = JoinSet::new();
    let (opener, _) = spawn_mux_no_reconnection(r, w, config, &mut spawner);
    mux_spawner.spawn(async move {
        let e = spawner.join_next().await.unwrap().unwrap();
        (listen_addr, e)
    });
    Ok((opener, addr))
}
#[derive(Debug)]
struct ConnectRequestMsg {
    pub listen_addr: SocketAddr,
    pub stream:
        tokio::sync::oneshot::Sender<io::Result<((StreamReader, StreamWriter), SocketAddrPair)>>,
}
fn connect_request_channel() -> (ConnectRequestTx, ConnectRequestRx) {
    let (tx, rx) = tokio::sync::mpsc::channel(1);
    let tx = ConnectRequestTx { tx };
    let rx = ConnectRequestRx { rx };
    (tx, rx)
}
#[derive(Debug)]
struct ConnectRequestTx {
    tx: tokio::sync::mpsc::Sender<ConnectRequestMsg>,
}
impl ConnectRequestTx {
    pub async fn send(
        &self,
        listen_addr: SocketAddr,
    ) -> io::Result<((StreamReader, StreamWriter), SocketAddrPair)> {
        let (tx, rx) = tokio::sync::oneshot::channel();
        let msg = ConnectRequestMsg {
            listen_addr,
            stream: tx,
        };
        self.tx.send(msg).await.unwrap();
        rx.await.unwrap()
    }
}
#[derive(Debug)]
struct ConnectRequestRx {
    rx: tokio::sync::mpsc::Receiver<ConnectRequestMsg>,
}
impl ConnectRequestRx {
    pub async fn recv(&mut self) -> Option<ConnectRequestMsg> {
        self.rx.recv().await
    }
}

#[derive(Debug, Clone, Copy)]
struct SocketAddrPair {
    pub local_addr: SocketAddr,
    pub peer_addr: SocketAddr,
}

#[derive(Debug)]
pub struct IoTcpMuxStream {
    stream: PollIo<StreamReader, StreamWriter>,
    addr: SocketAddrPair,
}
impl IoTcpMuxStream {
    fn new(stream: PollIo<StreamReader, StreamWriter>, addr: SocketAddrPair) -> Self {
        Self { stream, addr }
    }
}
impl AsyncWrite for IoTcpMuxStream {
    fn poll_write(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> std::task::Poll<Result<usize, io::Error>> {
        std::pin::Pin::new(&mut self.stream).poll_write(cx, buf)
    }

    fn poll_flush(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), io::Error>> {
        std::pin::Pin::new(&mut self.stream).poll_flush(cx)
    }

    fn poll_shutdown(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), io::Error>> {
        std::pin::Pin::new(&mut self.stream).poll_shutdown(cx)
    }
}
impl AsyncRead for IoTcpMuxStream {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<io::Result<()>> {
        std::pin::Pin::new(&mut self.stream).poll_read(cx, buf)
    }
}
impl IoStream for IoTcpMuxStream {}
impl IoAddr for IoTcpMuxStream {
    fn peer_addr(&self) -> io::Result<SocketAddr> {
        Ok(self.addr.peer_addr)
    }
    fn local_addr(&self) -> io::Result<SocketAddr> {
        Ok(self.addr.local_addr)
    }
}
