use std::{
    collections::{HashMap, hash_map},
    future::Future,
    io,
    net::SocketAddr,
    pin::Pin,
    time::Duration,
};

use common::{
    connect::ConnectorReset,
    stream::{AsConn, HasIoAddr, OwnIoStream},
};
use mux::{
    DeadControl, Initiation, MuxConfig, MuxError, StreamAccepter, StreamOpener, StreamReader,
    StreamWriter, spawn_mux_no_reconnection,
};
use tokio::{
    io::{AsyncRead, AsyncWrite},
    task::JoinSet,
};
use tracing::{debug, trace, warn};

pub fn server_mux_config() -> MuxConfig {
    MuxConfig {
        initiation: Initiation::Server,
        heartbeat_interval: Duration::from_secs(5),
        frame_reassembly: false,
    }
}

fn client_mux_config() -> MuxConfig {
    MuxConfig {
        initiation: Initiation::Client,
        heartbeat_interval: Duration::from_secs(5),
        frame_reassembly: false,
    }
}

pub async fn run_mux_accepter(
    mut accepter: StreamAccepter,
    addr: SocketAddrPair,
    mut handle_conn: impl FnMut(IoMuxStream<StreamReader, StreamWriter>),
) {
    loop {
        let (reader, writer) = match accepter.accept().await {
            Ok(stream) => stream,
            Err(DeadControl {}) => break,
        };
        let stream = tokio_chacha20::stream::DuplexStream::new(reader, writer);
        handle_conn(IoMuxStream::new(stream, addr));
    }
}

pub async fn run_mux_connector<R, W, Fut>(
    reset: ConnectorReset,
    mut connect_request_rx: ConnectRequestRx,
    mut connect: impl FnMut(SocketAddr) -> Fut,
) where
    R: AsyncRead + Unpin + Send + 'static,
    W: AsyncWrite + Unpin + Send + 'static,
    Fut: Future<Output = io::Result<((R, W), SocketAddrPair)>>,
{
    let mut openers: HashMap<SocketAddr, (StreamOpener, SocketAddrPair)> = HashMap::new();
    let mut mux_spawner: JoinSet<(SocketAddr, MuxError)> = JoinSet::new();
    let mut reset_notified = reset.0.waiter();
    loop {
        tokio::select! {
            () = reset_notified.notified() => {
                openers.clear();
                mux_spawner = JoinSet::new();
            }
            Some(result) = mux_spawner.join_next() => {
                match result {
                    Ok((addr, error)) => {
                        warn!(?error, ?addr, "MUX error");
                        openers.remove(&addr);
                    }
                    Err(error) if error.is_cancelled() => trace!(?error, "MUX task cancelled (normal shutdown/reset)"),
                    Err(error) => warn!(?error, "MUX supervision task failed to join"),
                }
            }
            result = connect_request_rx.recv() => {
                let Some(message) = result else { break };
                if let hash_map::Entry::Vacant(entry) = openers.entry(message.listen_addr) {
                    let ((reader, writer), addr) = match connect(message.listen_addr).await {
                        Ok(connection) => connection,
                        Err(error) => {
                            let _ = message.stream.send(Err(error));
                            continue;
                        }
                    };
                    let opener = build_opener(message.listen_addr, reader, writer, &mut mux_spawner).await;
                    entry.insert((opener, addr));
                }
                let (opener, addr) = openers.get(&message.listen_addr).unwrap();
                let stream = match opener.open().await {
                    Ok(stream) => stream,
                    Err(error) => {
                        let error = convert_open_err(error, addr);
                        let _ = message.stream.send(Err(error));
                        openers.remove(&message.listen_addr).unwrap();
                        continue;
                    }
                };
                let _ = message.stream.send(Ok((stream, *addr)));
            }
        }
    }
}

fn convert_open_err(error: mux::StreamOpenError, addr: &SocketAddrPair) -> io::Error {
    match error {
        mux::StreamOpenError::DeadControl(DeadControl {}) => io::Error::new(
            io::ErrorKind::ConnectionReset,
            format!("dead control; {addr:?}"),
        ),
        mux::StreamOpenError::ControlOpen(open_error) => match open_error {
            mux::ControlOpenError::TooManyOpenStreams(mux::TooManyOpenStreams {}) => {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("too many open streams; {addr:?}"),
                )
            }
            mux::ControlOpenError::DeadCentralIo(mux::DeadCentralIo { side }) => io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("dead central I/O; side: {side:?}; {addr:?}"),
            ),
        },
    }
}

async fn build_opener<R, W>(
    listen_addr: SocketAddr,
    reader: R,
    writer: W,
    mux_spawner: &mut JoinSet<(SocketAddr, MuxError)>,
) -> StreamOpener
where
    R: AsyncRead + Unpin + Send + 'static,
    W: AsyncWrite + Unpin + Send + 'static,
{
    let config = client_mux_config();
    let mut spawner = JoinSet::new();
    let (opener, _) = spawn_mux_no_reconnection(reader, writer, config, &mut spawner);
    mux_spawner.spawn(async move {
        let error = match spawner.join_next().await {
            Some(Ok(error)) => error,
            Some(Err(source)) if source.is_cancelled() => {
                debug!("build_opener: inner mux task cancelled");
                MuxError::TaskJoin {
                    task: "mux",
                    source,
                }
            }
            Some(Err(source)) => {
                debug!(?source, "build_opener: inner mux task join error");
                MuxError::TaskJoin {
                    task: "mux",
                    source,
                }
            }
            None => {
                debug!("build_opener: inner mux task produced no result");
                MuxError::TaskStopped { task: "mux" }
            }
        };
        (listen_addr, error)
    });
    opener
}

#[derive(Debug)]
struct ConnectRequestMsg {
    pub listen_addr: SocketAddr,
    pub stream:
        tokio::sync::oneshot::Sender<io::Result<((StreamReader, StreamWriter), SocketAddrPair)>>,
}

pub fn connect_request_channel() -> (ConnectRequestTx, ConnectRequestRx) {
    let (tx, rx) = tokio::sync::mpsc::channel(1);
    (ConnectRequestTx { tx }, ConnectRequestRx { rx })
}

#[derive(Debug)]
pub struct ConnectRequestTx {
    tx: tokio::sync::mpsc::Sender<ConnectRequestMsg>,
}

impl ConnectRequestTx {
    pub async fn send(
        &self,
        listen_addr: SocketAddr,
    ) -> io::Result<((StreamReader, StreamWriter), SocketAddrPair)> {
        let (tx, rx) = tokio::sync::oneshot::channel();
        self.tx
            .send(ConnectRequestMsg {
                listen_addr,
                stream: tx,
            })
            .await
            .unwrap();
        rx.await.unwrap()
    }
}

#[derive(Debug)]
pub struct ConnectRequestRx {
    rx: tokio::sync::mpsc::Receiver<ConnectRequestMsg>,
}

impl ConnectRequestRx {
    async fn recv(&mut self) -> Option<ConnectRequestMsg> {
        self.rx.recv().await
    }
}

#[derive(Debug, Clone, Copy)]
pub struct SocketAddrPair {
    pub local_addr: SocketAddr,
    pub peer_addr: SocketAddr,
}

#[derive(Debug)]
pub struct IoMuxStream<R, W> {
    stream: tokio_chacha20::stream::DuplexStream<R, W>,
    addr: SocketAddrPair,
}

impl<R, W> IoMuxStream<R, W> {
    pub fn new(stream: tokio_chacha20::stream::DuplexStream<R, W>, addr: SocketAddrPair) -> Self {
        Self { stream, addr }
    }
}

impl<R: AsyncRead + Unpin, W: Unpin> AsyncRead for IoMuxStream<R, W> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<io::Result<()>> {
        Pin::new(&mut self.stream).poll_read(cx, buf)
    }
}

impl<R: Unpin, W: AsyncWrite + Unpin> AsyncWrite for IoMuxStream<R, W> {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> std::task::Poll<io::Result<usize>> {
        Pin::new(&mut self.stream).poll_write(cx, buf)
    }

    fn poll_flush(
        mut self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<io::Result<()>> {
        Pin::new(&mut self.stream).poll_flush(cx)
    }

    fn poll_shutdown(
        mut self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<io::Result<()>> {
        Pin::new(&mut self.stream).poll_shutdown(cx)
    }
}

impl<R, W> AsConn for IoMuxStream<R, W> where Self: OwnIoStream {}

impl<R, W> OwnIoStream for IoMuxStream<R, W>
where
    R: std::fmt::Debug + Send + Sync + Unpin + 'static,
    W: std::fmt::Debug + Send + Sync + Unpin + 'static,
    Self: AsyncRead + AsyncWrite,
{
}

impl<R, W> HasIoAddr for IoMuxStream<R, W> {
    fn peer_addr(&self) -> io::Result<SocketAddr> {
        Ok(self.addr.peer_addr)
    }

    fn local_addr(&self) -> io::Result<SocketAddr> {
        Ok(self.addr.local_addr)
    }
}
