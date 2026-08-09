use std::{
    collections::{HashMap, hash_map},
    future::Future,
    io,
    net::SocketAddr,
    pin::Pin,
    time::Duration,
};

use common::{
    connect::ConnectorResetSignal,
    proto::{
        conn_handler::{stream::StreamProxyConnHandler, udp::UdpProxyConnHandler},
        context::UdpRuntime,
    },
    stream::{ConnParts, HasIoAddr, OwnIoStream, StreamServerHandleConn},
};
use mux::{
    DeadControl, Initiation, MuxConfig, MuxError, StreamAccepter, StreamOpener, StreamReader,
    StreamWriter, spawn_mux_no_reconnection,
};
use thiserror::Error;
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite},
    task::JoinSet,
};
use tracing::{debug, warn};

/// First byte of every mux stream opened against a mux proxy: `0` marks a
/// stream (TCP) flow, `1` marks a UDP datagram flow. Identical to the flow
/// kinds the reverse tunnel writes, so the wire format on a stream is the
/// same everywhere.
pub const STREAM_FLOW_KIND: u8 = 0;
pub const UDP_FLOW_KIND: u8 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MuxFlowKind {
    Stream,
    Udp,
}

/// Read the flow-kind byte that starts every mux-proxy stream.
///
/// The byte is required, so EOF or a timeout before it is an error; an
/// unknown kind is `InvalidData`. Datagram-flow peers that close before
/// sending anything surface their close on the first datagram read instead.
pub async fn read_flow_kind<Stream>(stream: &mut Stream) -> io::Result<MuxFlowKind>
where
    Stream: AsyncRead + Unpin,
{
    let kind = tokio::time::timeout(common::STREAM_IO_TIMEOUT, stream.read_u8())
        .await
        .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "mux flow kind timed out"))??;
    match kind {
        STREAM_FLOW_KIND => Ok(MuxFlowKind::Stream),
        UDP_FLOW_KIND => Ok(MuxFlowKind::Udp),
        kind => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("unsupported mux flow kind {kind}"),
        )),
    }
}

/// The combined stream + optional UDP proxy handler behind a mux proxy
/// listener. The two handlers share the listener's header/payload keys, and
/// bundling them lets a reload hot-swap both at once.
#[derive(Debug)]
pub struct MuxProxyHandler {
    pub stream: StreamProxyConnHandler,
    pub udp: Option<UdpProxyConnHandler>,
}
impl common::loading::HandleConn for MuxProxyHandler {}
impl StreamServerHandleConn for MuxProxyHandler {
    async fn handle_stream<Stream>(&self, stream: Stream)
    where
        Stream: ConnParts + std::fmt::Debug,
    {
        self.stream.handle_stream(stream).await;
    }
}
impl MuxProxyConnHandler for MuxProxyHandler {
    fn udp_proxy(&self) -> Option<&UdpProxyConnHandler> {
        self.udp.as_ref()
    }
}

/// The handler a mux proxy listener dispatches accepted streams to.
///
/// Implemented by [`MuxProxyHandler`]; generic over `ConnHandler` so the
/// mux servers stay independent of the concrete handler type.
pub trait MuxProxyConnHandler: StreamServerHandleConn + Send + Sync + 'static {
    fn udp_proxy(&self) -> Option<&UdpProxyConnHandler>;
}

/// Build the UDP proxy handler for a mux proxy listener from the same
/// header/payload keys as its stream handler, so both sides of the flow
/// share one wire-format/crypto config.
pub fn build_udp_proxy_handler(
    header_key: tokio_chacha20::config::ConfigBuilder,
    payload_key: Option<tokio_chacha20::config::ConfigBuilder>,
    udp_context: UdpRuntime,
    allow_loopback: bool,
) -> Result<Option<UdpProxyConnHandler>, MuxProxyUdpBuildError> {
    let header_crypto = header_key
        .build()
        .map_err(|e| MuxProxyUdpBuildError::HeaderCrypto(e.source.to_string()))?;
    let payload_crypto = match payload_key {
        Some(key) => Some(
            key.build()
                .map_err(|e| MuxProxyUdpBuildError::PayloadCrypto(e.source.to_string()))?,
        ),
        None => None,
    };
    Ok(Some(UdpProxyConnHandler::new(
        header_crypto,
        payload_crypto,
        udp_context,
        allow_loopback,
    )))
}

#[derive(Debug, Error)]
pub enum MuxProxyUdpBuildError {
    #[error("HeaderCrypto: {0}")]
    HeaderCrypto(String),
    #[error("PayloadCrypto: {0}")]
    PayloadCrypto(String),
}

pub fn server_mux_config() -> MuxConfig {
    MuxConfig {
        initiation: Initiation::Server,
        heartbeat_interval: Duration::from_secs(5),
        frame_reassembly: false,
    }
}

pub(crate) fn client_mux_config() -> MuxConfig {
    MuxConfig {
        initiation: Initiation::Client,
        heartbeat_interval: Duration::from_secs(5),
        frame_reassembly: false,
    }
}

pub async fn run_mux_accepter(
    mut accepter: StreamAccepter,
    _addr: SocketAddrPair,
    mut handle_conn: impl FnMut(
        (StreamReader, StreamWriter),
    ) -> Pin<Box<dyn Future<Output = ()> + Send>>,
) {
    loop {
        let (reader, writer) = match accepter.accept().await {
            Ok(stream) => stream,
            Err(DeadControl {}) => break,
        };
        handle_conn((reader, writer)).await;
    }
}

pub async fn run_mux_connector<R, W, Fut>(
    reset: ConnectorResetSignal,
    mut connect_request_rx: ConnectRequestRx,
    mut connect: impl FnMut(SocketAddr) -> Fut,
) where
    R: AsyncRead + Unpin + Send + 'static,
    W: AsyncWrite + Unpin + Send + 'static,
    Fut: Future<Output = io::Result<((R, W), SocketAddrPair)>>,
{
    let mut openers: HashMap<SocketAddr, (StreamOpener, SocketAddrPair)> = HashMap::new();
    let mut mux_spawner: JoinSet<(SocketAddr, MuxError)> = JoinSet::new();
    let mut reset_notified = reset.0.subscription();
    loop {
        tokio::select! {
            () = reset_notified.notified() => {
                openers.clear();
                mux_spawner = JoinSet::new();
            }
            Some(result) = mux_spawner.join_next() => {
                let (addr, error) = result.unwrap();
                warn!(?error, ?addr, "MUX error");
                openers.remove(&addr);
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
                    let opener =
                        build_opener(message.listen_addr, reader, writer, &mut mux_spawner).await;
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
            Some(result) => result.unwrap(),
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
            .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "MUX connector stopped"))?;
        rx.await.map_err(|_| {
            io::Error::new(
                io::ErrorKind::BrokenPipe,
                "MUX connector dropped connect response",
            )
        })?
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
pub struct AddressedMuxStream<R, W> {
    stream: tokio_chacha20::stream::DuplexStream<R, W>,
    addr: SocketAddrPair,
}

impl<R, W> AddressedMuxStream<R, W> {
    pub fn new(stream: tokio_chacha20::stream::DuplexStream<R, W>, addr: SocketAddrPair) -> Self {
        Self { stream, addr }
    }
}

impl<R: AsyncRead + Unpin, W: Unpin> AsyncRead for AddressedMuxStream<R, W> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<io::Result<()>> {
        Pin::new(&mut self.stream).poll_read(cx, buf)
    }
}

impl<R: Unpin, W: AsyncWrite + Unpin> AsyncWrite for AddressedMuxStream<R, W> {
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

impl<R, W> ConnParts for AddressedMuxStream<R, W> where Self: OwnIoStream {}

impl<R, W> OwnIoStream for AddressedMuxStream<R, W>
where
    R: std::fmt::Debug + Send + Sync + Unpin + 'static,
    W: std::fmt::Debug + Send + Sync + Unpin + 'static,
    Self: AsyncRead + AsyncWrite,
{
}

impl<R, W> HasIoAddr for AddressedMuxStream<R, W> {
    fn peer_addr(&self) -> io::Result<SocketAddr> {
        Ok(self.addr.peer_addr)
    }

    fn local_addr(&self) -> io::Result<SocketAddr> {
        Ok(self.addr.local_addr)
    }
}
