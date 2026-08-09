use std::{
    collections::{HashMap, hash_map},
    future::Future,
    io,
    net::SocketAddr,
    pin::Pin,
    sync::Arc,
    task::{Context, Poll},
    time::Duration,
};

use common::{
    connect::ConnectorResetSignal,
    proto::{
        conn_handler::{stream::StreamProxyConnHandler, udp::UdpProxyConnHandler},
        connect::udp::UdpConnection,
        context::UdpRuntime,
    },
    stream::{ConnParts, HasIoAddr, OwnIoStream, StreamServerHandleConn},
};
use metrics::counter;
use mux::{
    DeadControl, Initiation, MuxConfig, MuxError, StreamAccepter, StreamOpener, StreamReader,
    StreamWriter, spawn_mux_no_reconnection,
};
use thiserror::Error;
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt},
    task::JoinSet,
};
use tracing::{debug, warn};

/// First byte of every mux stream opened against a mux proxy / reverse
/// tunnel: `0` marks a stream (TCP) flow, `1` marks a UDP datagram flow.
/// The wire format is identical on every mux transport.
pub const STREAM_FLOW_KIND: u8 = 0;
pub const UDP_FLOW_KIND: u8 = 1;

/// The flow kinds a mux-proxy / reverse-tunnel stream starts with, as
/// written by [`write_flow_kind`] and read back by [`read_flow_kind`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MuxFlowKind {
    Stream,
    Udp,
}

impl MuxFlowKind {
    /// The wire byte that starts the stream: [`STREAM_FLOW_KIND`] or
    /// [`UDP_FLOW_KIND`].
    pub fn byte(self) -> u8 {
        match self {
            MuxFlowKind::Stream => STREAM_FLOW_KIND,
            MuxFlowKind::Udp => UDP_FLOW_KIND,
        }
    }
    /// Parse a wire byte read from the start of a stream.
    pub fn from_byte(byte: u8) -> Option<Self> {
        match byte {
            STREAM_FLOW_KIND => Some(MuxFlowKind::Stream),
            UDP_FLOW_KIND => Some(MuxFlowKind::Udp),
            _ => None,
        }
    }
}

/// Write the flow-kind byte that starts every mux-proxy / reverse-tunnel
/// stream, before any stream or datagram bytes.
pub async fn write_flow_kind<W>(writer: &mut W, kind: MuxFlowKind) -> io::Result<()>
where
    W: AsyncWrite + Unpin,
{
    writer.write_all(&[kind.byte()]).await
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
    MuxFlowKind::from_byte(kind).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("unsupported mux flow kind {kind}"),
        )
    })
}

/// A fatal error from a mux connector driver.
///
/// A connector driver is a process-lifetime task: it only exits when its
/// command loop or reset listener terminates, which leaves the connector
/// inert. Such an exit is fatal — the owning `server_tasks` `JoinSet` must
/// surface it so the server does not continue running with a dead
/// connector.
#[derive(Debug, Error)]
pub enum ConnectorDriverError {
    /// The connector's command loop exited. For the rtp_mux connector this
    /// only happens when its command sender is dropped, i.e. every
    /// `RtpMuxConnector` handle has gone away; the connector is inert.
    #[error("mux connector command loop exited; connector is inert")]
    ConnectorExited,
    /// The proxy reset listener exited after a [`ConnectorResetSignal`]
    /// reset failed, i.e. the connector refused or failed a reset.
    #[error("mux connector reset listener exited after a failed reset")]
    ResetListenerExited,
}

/// The driver for a mux connector (`TcpMuxConnector` / `RtpMuxConnector`).
///
/// It runs the connector's command loop (and, for rtp_mux, the reset
/// listener that tears down sessions on a [`ConnectorResetSignal`]). Spawn
/// it into the parent runtime's actively-reaped `JoinSet` so its exit is
/// observed and its drop aborts the connector task.
///
/// Its [`Future::Output`] is a [`ConnectorDriverError`]: the driver only
/// exits when one of its children terminates, which is fatal — the
/// connector is left inert and must not continue serving.
#[must_use = "the connector is inert until the driver is spawned"]
pub struct MuxConnectorDriver(Pin<Box<dyn Future<Output = ConnectorDriverError> + Send + 'static>>);

impl MuxConnectorDriver {
    pub fn new(future: impl Future<Output = ConnectorDriverError> + Send + 'static) -> Self {
        Self(Box::pin(future))
    }
}

impl Future for MuxConnectorDriver {
    type Output = ConnectorDriverError;
    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        self.0.as_mut().poll(cx)
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

impl From<rtp_mux::SocketAddrPair> for SocketAddrPair {
    fn from(value: rtp_mux::SocketAddrPair) -> Self {
        Self {
            local_addr: value.local_addr,
            peer_addr: value.peer_addr,
        }
    }
}

/// Dispatch one accepted mux stream by its flow-kind byte, for both the
/// tcpmux and rtpmux proxy servers.
///
/// `STREAM_FLOW_KIND` flows are wrapped by `wrap_stream` and handed to the
/// stream proxy handler; `UDP_FLOW_KIND` flows are framed with the same
/// `udp_mux` layout the reverse tunnel uses and handed to the UDP proxy
/// handler. The wire format on the stream is therefore identical to reverse
/// tunneling on every mux transport.
pub async fn dispatch_mux_flow<ConnHandler, S, Wrapped, WrapStream>(
    mut stream: S,
    addr: SocketAddrPair,
    conn_handler: Arc<ConnHandler>,
    wrap_stream: WrapStream,
    udp_flows_counter: &'static str,
) where
    ConnHandler: MuxProxyConnHandler,
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    Wrapped: ConnParts + std::fmt::Debug + Send + 'static,
    WrapStream: FnOnce(S, SocketAddrPair) -> Wrapped + Send + 'static,
{
    let kind = match read_flow_kind(&mut stream).await {
        Ok(kind) => kind,
        Err(error) => {
            warn!(?error, ?addr, "Failed to read mux flow kind");
            return;
        }
    };
    match kind {
        MuxFlowKind::Stream => {
            let stream = wrap_stream(stream, addr);
            conn_handler.handle_stream(stream).await;
        }
        MuxFlowKind::Udp => {
            let Some(udp_proxy) = conn_handler.udp_proxy() else {
                warn!(
                    ?addr,
                    "UDP mux flow received but the proxy has no UDP handler"
                );
                return;
            };
            counter!(udp_flows_counter).increment(1);
            let (reader, writer) = tokio::io::split(stream);
            let connection = UdpConnection::mux_io(reader, writer, addr.local_addr, addr.peer_addr);
            let (reader, writer) = connection.into_split();
            if let Err(error) = udp_proxy
                .handle_tunnel_flow(reader, writer, addr.peer_addr)
                .await
            {
                warn!(?error, ?addr, "Mux UDP flow failed");
            }
        }
    }
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
