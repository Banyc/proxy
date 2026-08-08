use crate::{
    addr::any_addr,
    connect::ConnectorConfig,
    error::AnyError,
    proto::{
        addr::RouteAddr,
        relay::udp::{UdpRecv, UdpSend},
    },
};
use async_trait::async_trait;
use mux::{StreamReader, StreamWriter, UdpMuxReader, UdpMuxWriter, udp_mux};
use std::{
    collections::HashMap,
    io,
    net::SocketAddr,
    sync::{
        Arc, RwLock, Weak,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};
use tokio::net::UdpSocket;

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
    pub fn mux(
        reader: StreamReader,
        writer: StreamWriter,
        local_addr: SocketAddr,
        peer_addr: SocketAddr,
    ) -> Self {
        let (reader, writer) = udp_mux(reader, writer);
        Self {
            read: UdpConnectionRead::Mux(reader),
            write: UdpConnectionWrite::Mux(writer),
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
#[derive(Debug)]
pub enum UdpConnectionRead {
    Socket(Arc<UdpSocket>),
    Mux(UdpMuxReader<StreamReader>),
}
impl UdpRecv for UdpConnectionRead {
    async fn trait_recv(&mut self, buf: &mut [u8]) -> Result<usize, AnyError> {
        match self {
            Self::Socket(socket) => socket.recv(buf).await.map_err(Into::into),
            Self::Mux(reader) => reader.recv(buf).await.map_err(Into::into),
        }
    }
}
#[derive(Debug)]
pub enum UdpConnectionWrite {
    Socket(Arc<UdpSocket>),
    Mux(UdpMuxWriter<StreamWriter>),
}
impl UdpSend for UdpConnectionWrite {
    async fn trait_send(&mut self, buf: &[u8]) -> Result<usize, AnyError> {
        match self {
            Self::Socket(socket) => socket.send(buf).await.map_err(Into::into),
            Self::Mux(writer) => writer.send(buf).await.map_err(Into::into),
        }
    }
}
#[async_trait]
pub trait NamedUdpConnect: std::fmt::Debug + Sync + Send + 'static {
    async fn connect(&self) -> io::Result<UdpConnection>;
    fn session_stats(&self) -> Option<String> {
        None
    }
}
type NamedUdpKey = (Arc<str>, Arc<str>);
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
    config: Arc<RwLock<ConnectorConfig>>,
    named: Arc<NamedUdpRegistry>,
}
impl UdpConnector {
    pub fn new(config: Arc<RwLock<ConnectorConfig>>) -> Self {
        Self {
            config,
            named: Arc::new(NamedUdpRegistry::new()),
        }
    }
    pub fn config(&self) -> &Arc<RwLock<ConnectorConfig>> {
        &self.config
    }
    pub async fn connect(&self, addr: SocketAddr) -> io::Result<UdpSocket> {
        let bind = self
            .config
            .read()
            .unwrap()
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
