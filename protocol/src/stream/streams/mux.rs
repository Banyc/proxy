use std::{
    collections::{HashMap, hash_map},
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
use tracing::warn;

const MAX_SEND_UNIT_SIZE: usize = 1024 * 4;

pub fn server_mux_config() -> MuxConfig {
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

pub async fn run_mux_accepter(
    mut accepter: StreamAccepter,
    addr: SocketAddrPair,
    mut handle_conn: impl FnMut(IoMuxStream),
) {
    loop {
        let (r, w) = match accepter.accept().await {
            Ok(x) => x,
            Err(DeadControl {}) => break,
        };
        let stream = tokio_chacha20::stream::DuplexStream::new(r, w);
        let stream = IoMuxStream::new(stream, addr);
        handle_conn(stream);
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
                    let ((r, w), addr) = match connect(msg.listen_addr).await {
                        Ok(x) => x,
                        Err(e) => {
                            let _ = msg.stream.send(Err(e));
                            continue;
                        }
                    };
                    let opener = build_opener(msg.listen_addr, r, w, &mut mux_spawner).await;
                    e.insert((opener, addr));
                }
                let (opener, addr) = openers.get(&msg.listen_addr).unwrap();
                let stream = match opener.open().await {
                    Ok(x) => x,
                    Err(e) => {
                        let e = convert_open_err(e, addr);
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
fn convert_open_err(err: mux::StreamOpenError, addr: &SocketAddrPair) -> io::Error {
    match err {
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
        let e = spawner.join_next().await.unwrap().unwrap();
        (listen_addr, e)
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
    let tx = ConnectRequestTx { tx };
    let rx = ConnectRequestRx { rx };
    (tx, rx)
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
        let msg = ConnectRequestMsg {
            listen_addr,
            stream: tx,
        };
        self.tx.send(msg).await.unwrap();
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
pub struct IoMuxStream {
    stream: tokio_chacha20::stream::DuplexStream<StreamReader, StreamWriter>,
    addr: SocketAddrPair,
}
impl IoMuxStream {
    pub fn new(
        stream: tokio_chacha20::stream::DuplexStream<StreamReader, StreamWriter>,
        addr: SocketAddrPair,
    ) -> Self {
        Self { stream, addr }
    }
}
impl AsyncWrite for IoMuxStream {
    fn poll_write(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> std::task::Poll<Result<usize, io::Error>> {
        let len = buf.len().min(MAX_SEND_UNIT_SIZE);
        let buf = &buf[..len];
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
impl AsyncRead for IoMuxStream {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<io::Result<()>> {
        std::pin::Pin::new(&mut self.stream).poll_read(cx, buf)
    }
}
impl AsConn for IoMuxStream {}
impl OwnIoStream for IoMuxStream {}
impl HasIoAddr for IoMuxStream {
    fn peer_addr(&self) -> io::Result<SocketAddr> {
        Ok(self.addr.peer_addr)
    }
    fn local_addr(&self) -> io::Result<SocketAddr> {
        Ok(self.addr.local_addr)
    }
}
