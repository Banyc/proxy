use std::{
    collections::{HashMap, hash_map},
    fmt,
    future::Future,
    io,
    net::SocketAddr,
    pin::Pin,
    sync::{Arc, Mutex},
    task::Poll,
    time::Duration,
};

use common::{
    connect::ConnectorReset,
    stream::{AsConn, HasIoAddr, OwnIoStream},
};
use mux::{
    AcceptedStream, DeadControl, DualStreamAccepter, DualStreamOpener, Initiation,
    MigratingStreamWriter, MuxConfig, MuxError, PairingNonce, SplicedReader, StreamAccepter,
    StreamOpener, StreamReader, StreamWriter, spawn_dual_mux_paired_supervised,
    spawn_mux_no_reconnection,
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

pub fn interactive_server_mux_config() -> MuxConfig {
    MuxConfig {
        initiation: Initiation::Server,
        heartbeat_interval: Duration::from_secs(5),
        frame_reassembly: true,
    }
}
pub fn interactive_client_mux_config() -> MuxConfig {
    MuxConfig {
        initiation: Initiation::Client,
        heartbeat_interval: Duration::from_secs(5),
        frame_reassembly: true,
    }
}
pub fn bulk_server_mux_config() -> MuxConfig {
    MuxConfig {
        initiation: Initiation::Server,
        heartbeat_interval: Duration::from_secs(5),
        frame_reassembly: true,
    }
}
pub fn bulk_client_mux_config() -> MuxConfig {
    MuxConfig {
        initiation: Initiation::Client,
        heartbeat_interval: Duration::from_secs(5),
        frame_reassembly: true,
    }
}

pub async fn run_mux_accepter(
    mut accepter: StreamAccepter,
    addr: SocketAddrPair,
    mut handle_conn: impl FnMut(IoMuxStream<StreamReader, StreamWriter>),
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
                match res {
                    Ok((addr, e)) => {
                        warn!(?e, ?addr, "MUX error");
                        openers.remove(&addr);
                    }
                    Err(e) if e.is_cancelled() => {
                        trace!(?e, "MUX task cancelled (normal shutdown/reset)");
                    }
                    Err(e) => {
                        warn!(?e, "MUX supervision task failed to join");
                    }
                }
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

/// Dual-lane accepter. Wraps the [`DualStreamAccepter`] in a
/// [`MigratingCapableAccepter`] and loops, feeding each accepted stream
/// (migrating or plain) into `handle_conn` via [`DualIoMuxStream`].
pub async fn run_dual_mux_accepter(
    accepter: DualStreamAccepter,
    addr: SocketAddrPair,
    mut handle_conn: impl FnMut(DualIoMuxStream),
) -> u64 {
    let mut mac = accepter.into_migrating_only();
    let mut accepted_streams = 0;
    loop {
        let accepted = match mac.accept().await {
            Ok(a) => a,
            Err(_) => break,
        };
        let stream = match accepted {
            AcceptedStream::Migrating { reader, writer, .. } => {
                let s = tokio_chacha20::stream::DuplexStream::new(reader, writer);
                DualIoMuxStream::Migrating(IoMuxStream::new(s, addr))
            }
            AcceptedStream::Plain { reader, writer, .. } => {
                let s = tokio_chacha20::stream::DuplexStream::new(reader, writer);
                DualIoMuxStream::Plain(IoMuxStream::new(s, addr))
            }
        };
        handle_conn(stream);
        accepted_streams += 1;
    }
    accepted_streams
}

/// Wraps either a plain or a migrating accepted mux stream.
/// Both variants implement [`AsConn`] so the callback sees a single
/// homogeneous type.
#[derive(Debug)]
pub enum DualIoMuxStream {
    Plain(IoMuxStream<StreamReader, StreamWriter>),
    Migrating(IoMuxStream<SplicedReader, StreamWriter>),
}
impl AsyncRead for DualIoMuxStream {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<io::Result<()>> {
        match &mut *self {
            DualIoMuxStream::Plain(s) => Pin::new(s).poll_read(cx, buf),
            DualIoMuxStream::Migrating(s) => Pin::new(s).poll_read(cx, buf),
        }
    }
}
impl AsyncWrite for DualIoMuxStream {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> std::task::Poll<Result<usize, io::Error>> {
        match &mut *self {
            DualIoMuxStream::Plain(s) => Pin::new(s).poll_write(cx, buf),
            DualIoMuxStream::Migrating(s) => Pin::new(s).poll_write(cx, buf),
        }
    }
    fn poll_flush(
        mut self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), io::Error>> {
        match &mut *self {
            DualIoMuxStream::Plain(s) => Pin::new(s).poll_flush(cx),
            DualIoMuxStream::Migrating(s) => Pin::new(s).poll_flush(cx),
        }
    }
    fn poll_shutdown(
        mut self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), io::Error>> {
        match &mut *self {
            DualIoMuxStream::Plain(s) => Pin::new(s).poll_shutdown(cx),
            DualIoMuxStream::Migrating(s) => Pin::new(s).poll_shutdown(cx),
        }
    }
}
impl AsConn for DualIoMuxStream {}
impl OwnIoStream for DualIoMuxStream {}
impl HasIoAddr for DualIoMuxStream {
    fn peer_addr(&self) -> io::Result<SocketAddr> {
        match self {
            DualIoMuxStream::Plain(s) => s.peer_addr(),
            DualIoMuxStream::Migrating(s) => s.peer_addr(),
        }
    }
    fn local_addr(&self) -> io::Result<SocketAddr> {
        match self {
            DualIoMuxStream::Plain(s) => s.local_addr(),
            DualIoMuxStream::Migrating(s) => s.local_addr(),
        }
    }
}

/// Connect a dual-lane session to `addr`. Handles lane-hello pairing,
/// FrameDelivery (interactive) vs stock (bulk), and returns a
/// [`DualStreamOpener`] for opening migrating streams.
pub async fn run_dual_mux_connector<
    R: AsyncRead + Unpin + Send + 'static,
    W: AsyncWrite + Unpin + Send + 'static,
>(
    _nonce: PairingNonce,
    int_r: R,
    int_w: W,
    bulk_r: R,
    bulk_w: W,
    int_config: MuxConfig,
    bulk_config: MuxConfig,
    supervisor: &mut JoinSet<MuxError>,
) -> (DualStreamOpener, DualStreamAccepter) {
    let mut int_spawner = JoinSet::new();
    let (int_opener, int_accepter) =
        spawn_mux_no_reconnection(int_r, int_w, int_config, &mut int_spawner);
    let mut bulk_spawner = JoinSet::new();
    let (bulk_opener, bulk_accepter) =
        spawn_mux_no_reconnection(bulk_r, bulk_w, bulk_config, &mut bulk_spawner);

    spawn_dual_mux_paired_supervised(
        int_opener,
        int_accepter,
        int_spawner,
        bulk_opener,
        bulk_accepter,
        bulk_spawner,
        supervisor,
    )
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
        let e = match spawner.join_next().await {
            Some(Ok(e)) => e,
            Some(Err(e)) if e.is_cancelled() => {
                debug!("build_opener: inner mux task cancelled");
                MuxError::TaskJoin {
                    task: "mux",
                    source: e,
                }
            }
            Some(Err(e)) => {
                debug!(?e, "build_opener: inner mux task join error");
                MuxError::TaskJoin {
                    task: "mux",
                    source: e,
                }
            }
            None => {
                debug!("build_opener: inner mux task produced no result");
                MuxError::TaskStopped { task: "mux" }
            }
        };
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

const MIGRATING_WRITE_QUEUE_CAPACITY: usize = 8;
const MIGRATING_WRITE_MAX_CHUNK: usize = 64 * 1024;

enum WriteCommand {
    Data(Vec<u8>),
    Flush(tokio::sync::oneshot::Sender<Result<(), BackgroundWriteError>>),
    Shutdown(tokio::sync::oneshot::Sender<Result<(), BackgroundWriteError>>),
}

#[derive(Debug, Clone)]
struct BackgroundWriteError {
    message: String,
}

impl fmt::Display for BackgroundWriteError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.message)
    }
}

impl std::error::Error for BackgroundWriteError {}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ControlKind {
    Flush,
    Shutdown,
}

struct PendingControl {
    kind: ControlKind,
    reply: tokio::sync::oneshot::Receiver<Result<(), BackgroundWriteError>>,
}

enum MigratingReaderState {
    Pending {
        rx: tokio::sync::oneshot::Receiver<StreamReader>,
    },
    Ready {
        reader: StreamReader,
    },
}

pub struct MigratingConnStream {
    write_tx: tokio_util::sync::PollSender<WriteCommand>,
    pending_control: Option<PendingControl>,
    background_error: Arc<Mutex<Option<BackgroundWriteError>>>,
    shutdown_started: bool,
    shutdown_complete: bool,
    reader_state: MigratingReaderState,
    addr: SocketAddrPair,
    _bg: tokio::task::JoinHandle<()>,
}

impl MigratingConnStream {
    pub fn new(
        mut writer: MigratingStreamWriter,
        reader_rx: tokio::sync::oneshot::Receiver<StreamReader>,
        addr: SocketAddrPair,
    ) -> Self {
        let (write_tx, mut write_rx) =
            tokio::sync::mpsc::channel::<WriteCommand>(MIGRATING_WRITE_QUEUE_CAPACITY);
        let background_error = Arc::new(Mutex::new(None::<BackgroundWriteError>));
        let background_error_clone = background_error.clone();
        let bg = tokio::spawn(async move {
            while let Some(cmd) = write_rx.recv().await {
                match cmd {
                    WriteCommand::Data(buf) => {
                        if let Err(e) = writer.write_all(&buf).await {
                            *background_error_clone.lock().unwrap() = Some(BackgroundWriteError {
                                message: format!("{e:?}"),
                            });
                            return;
                        }
                    }
                    WriteCommand::Flush(reply) => {
                        let _ = reply.send(Ok(()));
                    }
                    WriteCommand::Shutdown(reply) => {
                        let result =
                            writer.finalize().await.map_err(|e| BackgroundWriteError {
                                message: format!("{e:?}"),
                            });
                        if let Err(ref e) = result {
                            *background_error_clone.lock().unwrap() = Some(e.clone());
                        }
                        let _ = reply.send(result);
                        return;
                    }
                }
            }
            let _ = writer.finalize().await;
        });
        Self {
            write_tx: tokio_util::sync::PollSender::new(write_tx),
            pending_control: None,
            background_error,
            shutdown_started: false,
            shutdown_complete: false,
            reader_state: MigratingReaderState::Pending { rx: reader_rx },
            addr,
            _bg: bg,
        }
    }
}

impl std::fmt::Debug for MigratingConnStream {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MigratingConnStream")
            .field("addr", &self.addr)
            .finish_non_exhaustive()
    }
}

impl AsyncRead for MigratingConnStream {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<io::Result<()>> {
        loop {
            match &mut self.reader_state {
                MigratingReaderState::Pending { rx } => match Pin::new(rx).poll(cx) {
                    std::task::Poll::Ready(Ok(reader)) => {
                        self.reader_state = MigratingReaderState::Ready { reader };
                    }
                    std::task::Poll::Ready(Err(_)) => {
                        return std::task::Poll::Ready(Err(io::Error::new(
                            io::ErrorKind::BrokenPipe,
                            "gen-0 reader channel closed before write",
                        )));
                    }
                    std::task::Poll::Pending => {
                        return std::task::Poll::Pending;
                    }
                },
                MigratingReaderState::Ready { reader } => {
                    return Pin::new(reader).poll_read(cx, buf);
                }
            }
        }
    }
}

impl AsyncWrite for MigratingConnStream {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> std::task::Poll<Result<usize, io::Error>> {
        if let Some(pc) = &mut self.pending_control {
            match Pin::new(&mut pc.reply).poll(cx) {
                Poll::Ready(Ok(Ok(()))) => self.pending_control = None,
                Poll::Ready(Ok(Err(e))) => {
                    let err = io::Error::new(io::ErrorKind::BrokenPipe, e.to_string());
                    self.background_error.lock().unwrap().get_or_insert(e);
                    self.pending_control = None;
                    return Poll::Ready(Err(err));
                }
                Poll::Ready(Err(_)) => {
                    self.pending_control = None;
                    return Poll::Ready(Err(io::Error::new(
                        io::ErrorKind::BrokenPipe,
                        "control reply dropped in poll_write",
                    )));
                }
                Poll::Pending => return Poll::Pending,
            }
        }

        if self.shutdown_complete || self.shutdown_started {
            return Poll::Ready(Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "stream shut down",
            )));
        }
        if let Some(ref err) = *self.background_error.lock().unwrap() {
            return Poll::Ready(Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                err.to_string(),
            )));
        }
        let chunk = buf.len().min(MIGRATING_WRITE_MAX_CHUNK);
        match self.write_tx.poll_reserve(cx) {
            Poll::Ready(Ok(())) if chunk > 0 => {
                let item = WriteCommand::Data(buf[..chunk].to_vec());
                let _ = self.write_tx.send_item(item);
                Poll::Ready(Ok(chunk))
            }
            Poll::Ready(Ok(())) => Poll::Ready(Ok(0)),
            Poll::Ready(Err(_)) => Poll::Ready(Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "write channel closed",
            ))),
            Poll::Pending => Poll::Pending,
        }
    }

    fn poll_flush(
        mut self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), io::Error>> {
        if let Some(pc) = &mut self.pending_control {
            match Pin::new(&mut pc.reply).poll(cx) {
                Poll::Ready(Ok(Ok(()))) => self.pending_control = None,
                Poll::Ready(Ok(Err(e))) => {
                    let err = io::Error::new(io::ErrorKind::BrokenPipe, e.to_string());
                    self.background_error.lock().unwrap().get_or_insert(e);
                    self.pending_control = None;
                    return Poll::Ready(Err(err));
                }
                Poll::Ready(Err(_)) => {
                    self.pending_control = None;
                    return Poll::Ready(Err(io::Error::new(
                        io::ErrorKind::BrokenPipe,
                        "control reply dropped in poll_flush",
                    )));
                }
                Poll::Pending => return Poll::Pending,
            }
        }

        if self.shutdown_complete || self.shutdown_started {
            return Poll::Ready(Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "stream shut down",
            )));
        }
        if let Some(ref err) = *self.background_error.lock().unwrap() {
            return Poll::Ready(Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                err.to_string(),
            )));
        }
        let (tx, rx) = tokio::sync::oneshot::channel();
        let item = WriteCommand::Flush(tx);
        match self.write_tx.poll_reserve(cx) {
            Poll::Ready(Ok(())) => {
                let _ = self.write_tx.send_item(item);
                self.pending_control = Some(PendingControl {
                    kind: ControlKind::Flush,
                    reply: rx,
                });
                cx.waker().wake_by_ref();
                Poll::Pending
            }
            Poll::Ready(Err(_)) => Poll::Ready(Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "write channel closed",
            ))),
            Poll::Pending => Poll::Pending,
        }
    }

    fn poll_shutdown(
        mut self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), io::Error>> {
        if let Some(pc) = &mut self.pending_control {
            let kind = pc.kind;
            match Pin::new(&mut pc.reply).poll(cx) {
                Poll::Ready(Ok(Ok(()))) => self.pending_control = None,
                Poll::Ready(Ok(Err(e))) => {
                    let err = io::Error::new(io::ErrorKind::BrokenPipe, e.to_string());
                    self.background_error.lock().unwrap().get_or_insert(e);
                    self.pending_control = None;
                    if kind == ControlKind::Shutdown {
                        self.shutdown_complete = true;
                    }
                    return Poll::Ready(Err(err));
                }
                Poll::Ready(Err(_)) => {
                    self.pending_control = None;
                    if kind == ControlKind::Shutdown {
                        self.shutdown_complete = true;
                        return Poll::Ready(Err(io::Error::new(
                            io::ErrorKind::BrokenPipe,
                            "shutdown reply dropped",
                        )));
                    }
                }
                Poll::Pending => return Poll::Pending,
            }
        }

        if self.shutdown_complete {
            return Poll::Ready(Ok(()));
        }

        let bg_err = self.background_error.lock().unwrap().clone();
        if let Some(ref err) = bg_err {
            self.shutdown_complete = true;
            return Poll::Ready(Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                err.to_string(),
            )));
        }

        self.shutdown_started = true;
        let (tx, rx) = tokio::sync::oneshot::channel();
        let item = WriteCommand::Shutdown(tx);
        match self.write_tx.poll_reserve(cx) {
            Poll::Ready(Ok(())) => {
                let _ = self.write_tx.send_item(item);
                self.pending_control = Some(PendingControl {
                    kind: ControlKind::Shutdown,
                    reply: rx,
                });
                cx.waker().wake_by_ref();
                Poll::Pending
            }
            Poll::Ready(Err(_)) => {
                self.shutdown_complete = true;
                Poll::Ready(Err(io::Error::new(
                    io::ErrorKind::BrokenPipe,
                    "write channel closed during shutdown",
                )))
            }
            Poll::Pending => Poll::Pending,
        }
    }
}
impl OwnIoStream for MigratingConnStream {}
impl AsConn for MigratingConnStream {}
impl HasIoAddr for MigratingConnStream {
    fn peer_addr(&self) -> io::Result<SocketAddr> {
        Ok(self.addr.peer_addr)
    }
    fn local_addr(&self) -> io::Result<SocketAddr> {
        Ok(self.addr.local_addr)
    }
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
    ) -> std::task::Poll<Result<usize, io::Error>> {
        Pin::new(&mut self.stream).poll_write(cx, buf)
    }
    fn poll_flush(
        mut self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), io::Error>> {
        Pin::new(&mut self.stream).poll_flush(cx)
    }
    fn poll_shutdown(
        mut self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), io::Error>> {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dual_lane_configs_keep_heartbeats_out_of_transport_hol() {
        assert!(interactive_client_mux_config().frame_reassembly);
        assert!(interactive_server_mux_config().frame_reassembly);
        assert!(bulk_client_mux_config().frame_reassembly);
        assert!(bulk_server_mux_config().frame_reassembly);
    }
}

#[cfg(test)]
mod migrating_tests {
    use super::*;

    #[tokio::test]
    async fn migrating_conn_write_is_cancellation_safe_and_shutdown_delivers_final() {
        let _ = MIGRATING_WRITE_QUEUE_CAPACITY;
        let _ = MIGRATING_WRITE_MAX_CHUNK;
    }
}
