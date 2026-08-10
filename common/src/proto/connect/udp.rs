use crate::{
    addr::any_addr,
    connect::ConnectorConfigReader,
    error::AnyError,
    proto::{
        addr::RouteAddr,
        relay::udp::{UdpRecv, UdpSend},
    },
};
use async_trait::async_trait;
use mux::{UdpMuxReader, UdpMuxWriter, udp_mux};
use std::{
    collections::HashMap,
    fmt, io,
    net::SocketAddr,
    sync::{
        Arc, RwLock, Weak,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};
use tokio::{
    io::{AsyncRead, AsyncWrite},
    net::UdpSocket,
};

#[derive(Debug)]
pub struct UdpConnection {
    read: UdpConnectionRead,
    write: UdpConnectionWrite,
    local_addr: Option<SocketAddr>,
    peer_addr: Option<SocketAddr>,
}
impl UdpConnection {
    pub fn socket(socket: UdpSocket) -> Self {
        let local_addr = socket.local_addr().ok();
        let peer_addr = socket.peer_addr().ok();
        let socket = Arc::new(socket);
        Self {
            read: UdpConnectionRead::Socket(Arc::clone(&socket)),
            write: UdpConnectionWrite::Socket(socket),
            local_addr,
            peer_addr,
        }
    }
    /// A datagram flow over a multiplexed byte stream, framed with
    /// `udp_mux` (2-byte big-endian length prefix + payload per datagram)
    /// exactly like reverse tunneling. The read/write halves are boxed so
    /// any `AsyncRead` + `AsyncWrite` pair (a `mux` stream, an `rtp_mux`
    /// client stream, ...) can carry a UDP flow with the same wire format.
    pub fn mux_io<R, W>(reader: R, writer: W, local_addr: SocketAddr, peer_addr: SocketAddr) -> Self
    where
        R: AsyncRead + Unpin + Send + 'static,
        W: AsyncWrite + Unpin + Send + 'static,
    {
        let (reader, writer) = udp_mux(
            Box::new(reader) as Box<dyn AsyncRead + Unpin + Send>,
            Box::new(writer) as Box<dyn AsyncWrite + Unpin + Send>,
        );
        Self {
            read: UdpConnectionRead::Io(reader),
            write: UdpConnectionWrite::Io(writer),
            local_addr: Some(local_addr),
            peer_addr: Some(peer_addr),
        }
    }
    pub fn into_split(self) -> (UdpConnectionRead, UdpConnectionWrite) {
        (self.read, self.write)
    }
    pub fn local_addr(&self) -> Option<SocketAddr> {
        self.local_addr
    }
    pub fn peer_addr(&self) -> Option<SocketAddr> {
        self.peer_addr
    }
}
pub enum UdpConnectionRead {
    Socket(Arc<UdpSocket>),
    Io(UdpMuxReader<Box<dyn AsyncRead + Unpin + Send>>),
}
impl fmt::Debug for UdpConnectionRead {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Socket(socket) => f.debug_tuple("Socket").field(socket).finish(),
            Self::Io(_) => f.debug_tuple("Io").field(&"<boxed io>").finish(),
        }
    }
}
impl UdpRecv for UdpConnectionRead {
    async fn trait_recv(&mut self, buf: &mut [u8]) -> Result<usize, AnyError> {
        match self {
            Self::Socket(socket) => socket.recv(buf).await.map_err(Into::into),
            Self::Io(reader) => reader.recv(buf).await.map_err(Into::into),
        }
    }
}
pub enum UdpConnectionWrite {
    Socket(Arc<UdpSocket>),
    Io(UdpMuxWriter<Box<dyn AsyncWrite + Unpin + Send>>),
}
impl fmt::Debug for UdpConnectionWrite {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Socket(socket) => f.debug_tuple("Socket").field(socket).finish(),
            Self::Io(_) => f.debug_tuple("Io").field(&"<boxed io>").finish(),
        }
    }
}
impl UdpSend for UdpConnectionWrite {
    async fn trait_send(&mut self, buf: &[u8]) -> Result<usize, AnyError> {
        match self {
            Self::Socket(socket) => socket.send(buf).await.map_err(Into::into),
            Self::Io(writer) => writer.send(buf).await.map_err(Into::into),
        }
    }
    async fn trait_shutdown(&mut self) -> Result<(), AnyError> {
        match self {
            // A connected datagram socket cannot half-close; nothing to do.
            Self::Socket(_) => Ok(()),
            // Gracefully close the mux stream so the peer sees a clean EOF
            // at the datagram boundary instead of idling the flow out.
            Self::Io(writer) => writer.shutdown().await.map_err(Into::into),
        }
    }
}

/// A dialer that opens a UDP datagram flow over a multiplexed byte stream.
///
/// Implemented by the `tcpmux`/`rtpmux` stream connectors: they dial the
/// remote mux proxy, open a fresh mux stream, write the UDP flow-kind byte,
/// and frame datagrams with the same `udp_mux` layout the reverse tunnel
/// uses, so the wire format on the stream is identical in both cases.
#[async_trait]
pub trait UdpMuxDialer: std::fmt::Debug + Sync + Send + 'static {
    async fn dial_udp(&self, addr: SocketAddr) -> io::Result<UdpConnection>;
}
#[async_trait]
pub trait NamedUdpConnect: std::fmt::Debug + Sync + Send + 'static {
    async fn connect(&self) -> io::Result<UdpConnection>;
    fn session_stats(&self) -> Option<String> {
        None
    }
}
type NamedUdpKey = (Arc<str>, Arc<str>);
type UdpMuxDialerMap = HashMap<Arc<str>, Arc<dyn UdpMuxDialer>>;
#[derive(Debug)]
struct NamedUdpEntry {
    generation: u64,
    connector: Arc<dyn NamedUdpConnect>,
}
#[derive(Debug)]
struct NamedUdpRegistry {
    entries: RwLock<HashMap<NamedUdpKey, NamedUdpEntry>>,
    next_generation: AtomicU64,
    changed: tokio::sync::watch::Sender<u64>,
}
impl NamedUdpRegistry {
    fn new() -> Self {
        let (changed, _) = tokio::sync::watch::channel(0);
        Self {
            entries: RwLock::new(HashMap::new()),
            next_generation: AtomicU64::new(1),
            changed,
        }
    }
    fn register(
        self: &Arc<Self>,
        protocol: Arc<str>,
        name: Arc<str>,
        connector: Arc<dyn NamedUdpConnect>,
    ) -> NamedUdpRegistration {
        let generation = self.next_generation.fetch_add(1, Ordering::Relaxed);
        let key = (protocol, name);
        self.entries.write().unwrap().insert(
            key.clone(),
            NamedUdpEntry {
                generation,
                connector,
            },
        );
        self.changed
            .send_modify(|value| *value = value.wrapping_add(1));
        NamedUdpRegistration {
            registry: Arc::downgrade(self),
            key,
            generation,
        }
    }
    async fn connect(&self, protocol: &str, name: &str) -> io::Result<UdpConnection> {
        let mut changed = self.changed.subscribe();
        loop {
            let connector = self
                .entries
                .read()
                .unwrap()
                .get(&(Arc::from(protocol), Arc::from(name)))
                .map(|entry| Arc::clone(&entry.connector));
            if let Some(connector) = connector {
                return connector.connect().await;
            }
            changed.changed().await.map_err(|_| {
                io::Error::new(
                    io::ErrorKind::NotConnected,
                    "named UDP connector registry closed",
                )
            })?;
        }
    }
    fn session_stats(&self, protocol: &str, name: &str) -> Option<String> {
        self.entries
            .read()
            .unwrap()
            .get(&(Arc::from(protocol), Arc::from(name)))
            .and_then(|entry| entry.connector.session_stats())
    }
}
#[derive(Debug)]
pub struct NamedUdpRegistration {
    registry: Weak<NamedUdpRegistry>,
    key: NamedUdpKey,
    generation: u64,
}
impl Drop for NamedUdpRegistration {
    fn drop(&mut self) {
        let Some(registry) = self.registry.upgrade() else {
            return;
        };
        let mut entries = registry.entries.write().unwrap();
        if entries
            .get(&self.key)
            .is_some_and(|entry| entry.generation == self.generation)
        {
            entries.remove(&self.key);
            drop(entries);
            registry
                .changed
                .send_modify(|value| *value = value.wrapping_add(1));
        }
    }
}

#[derive(Debug, Clone)]
pub struct UdpConnector {
    config: ConnectorConfigReader,
    named: Arc<NamedUdpRegistry>,
    /// Per-protocol dialers that carry datagram flows over a multiplexed
    /// byte stream (`tcpmux`, `rtpmux`, `rtpmuxfec`). Registered once at
    /// runtime build time, alongside the stream connector table.
    dialers: Arc<RwLock<UdpMuxDialerMap>>,
}
impl UdpConnector {
    pub fn new(config: ConnectorConfigReader) -> Self {
        Self {
            config,
            named: Arc::new(NamedUdpRegistry::new()),
            dialers: Arc::new(RwLock::new(HashMap::new())),
        }
    }
    pub fn register_dialer(&self, protocol: Arc<str>, dialer: Arc<dyn UdpMuxDialer>) {
        self.dialers.write().unwrap().insert(protocol, dialer);
    }
    pub async fn connect(&self, addr: SocketAddr) -> io::Result<UdpSocket> {
        let bind = self
            .config
            .current()
            .bind
            .get_matched(&addr.ip())
            .map(|ip| SocketAddr::new(ip, 0))
            .unwrap_or_else(|| any_addr(&addr.ip()));
        let socket = UdpSocket::bind(bind).await?;
        socket.connect(addr).await?;
        Ok(socket)
    }
    pub async fn connect_route(
        &self,
        addr: &RouteAddr,
        timeout: Duration,
    ) -> io::Result<UdpConnection> {
        if let Some((_, name)) = addr.reverse_tunnel() {
            return tokio::time::timeout(timeout, self.named.connect(&addr.protocol, name))
                .await
                .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "timed out"))?;
        }
        let dialer = {
            let dialers = self.dialers.read().unwrap();
            dialers.get(&addr.protocol).cloned()
        };
        if let Some(dialer) = dialer {
            let sock_addr = *addr.address.to_socket_addrs().await?.first();
            return tokio::time::timeout(timeout, dialer.dial_udp(sock_addr))
                .await
                .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "timed out"))?;
        }
        let sock_addr = *addr.address.to_socket_addrs().await?.first();
        self.connect(sock_addr).await.map(UdpConnection::socket)
    }
    pub fn register_named(
        &self,
        protocol: Arc<str>,
        name: Arc<str>,
        connector: Arc<dyn NamedUdpConnect>,
    ) -> NamedUdpRegistration {
        self.named.register(protocol, name, connector)
    }
    pub fn named_session_stats(&self, protocol: &str, name: &str) -> Option<String> {
        self.named.session_stats(protocol, name)
    }
}
