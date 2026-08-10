use std::{
    collections::HashMap,
    io,
    net::SocketAddr,
    ops::Deref,
    sync::{
        Arc, RwLock, Weak,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};

use async_trait::async_trait;

use crate::{connect::ConnectorConfigReader, stream::ConnParts};

#[async_trait]
pub trait StreamConnect: std::fmt::Debug + Sync + Send + 'static {
    async fn connect(&self, addr: SocketAddr) -> io::Result<Box<dyn ConnParts>>;
    fn reset_addr(&self, _addr: SocketAddr) {}
    fn reoptimize(&self, _addr: SocketAddr) {}
    fn session_stats(&self, _addr: SocketAddr) -> Option<String> {
        None
    }
    fn reports_session_stats(&self) -> bool {
        false
    }
}
pub trait StreamConnectExt: StreamConnect {
    fn timed_connect(
        &self,
        addr: SocketAddr,
        timeout: Duration,
    ) -> impl Future<Output = io::Result<Box<dyn ConnParts>>> + Send
    where
        Self: Sync,
    {
        async move {
            let res = tokio::time::timeout(timeout, self.connect(addr)).await;
            match res {
                Ok(res) => res,
                Err(_) => Err(io::Error::new(io::ErrorKind::TimedOut, "Timed out")),
            }
        }
    }
}
impl<T: StreamConnect + ?Sized> StreamConnectExt for T {}

#[async_trait]
pub trait NamedStreamConnect: std::fmt::Debug + Sync + Send + 'static {
    async fn connect(&self) -> io::Result<Box<dyn ConnParts>>;
    fn session_stats(&self) -> Option<String> {
        None
    }
}

type NamedStreamKey = (Arc<str>, Arc<str>);

#[derive(Debug)]
struct NamedStreamEntry {
    generation: u64,
    connector: Arc<dyn NamedStreamConnect>,
}

#[derive(Debug)]
struct NamedStreamRegistry {
    entries: RwLock<HashMap<NamedStreamKey, NamedStreamEntry>>,
    next_generation: AtomicU64,
    changed: tokio::sync::watch::Sender<u64>,
}
impl NamedStreamRegistry {
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
        connector: Arc<dyn NamedStreamConnect>,
    ) -> NamedStreamRegistration {
        let generation = self.next_generation.fetch_add(1, Ordering::Relaxed);
        let key = (protocol, name);
        self.entries.write().unwrap().insert(
            key.clone(),
            NamedStreamEntry {
                generation,
                connector,
            },
        );
        self.changed.send_modify(|value| {
            *value = value.wrapping_add(1);
        });
        NamedStreamRegistration {
            registry: Arc::downgrade(self),
            key,
            generation,
        }
    }
    async fn connect(&self, protocol: &str, name: &str) -> io::Result<Box<dyn ConnParts>> {
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
                    "named connector registry closed",
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
pub struct NamedStreamRegistration {
    registry: Weak<NamedStreamRegistry>,
    key: NamedStreamKey,
    generation: u64,
}
impl Drop for NamedStreamRegistration {
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
            registry.changed.send_modify(|value| {
                *value = value.wrapping_add(1);
            });
        }
    }
}

#[derive(Debug)]
pub struct StreamConnectorTable {
    config: ConnectorConfigReader,
    connectors: HashMap<Arc<str>, Arc<dyn StreamConnect>>,
    named: Arc<NamedStreamRegistry>,
}
impl StreamConnectorTable {
    pub fn new(
        config: ConnectorConfigReader,
        connectors: HashMap<Arc<str>, Arc<dyn StreamConnect>>,
    ) -> Self {
        Self {
            config,
            connectors,
            named: Arc::new(NamedStreamRegistry::new()),
        }
    }
    pub fn bind_addr_for(&self, peer: SocketAddr) -> SocketAddr {
        self.config
            .current()
            .bind
            .get_matched(&peer.ip())
            .map(|ip| SocketAddr::new(ip, 0))
            .unwrap_or_else(|| crate::addr::any_addr(&peer.ip()))
    }
}
impl StreamConnectorTable {
    pub async fn timed_connect_any(
        &self,
        stream_type: &str,
        addrs: impl IntoIterator<Item = SocketAddr>,
        timeout: Duration,
    ) -> io::Result<(Box<dyn ConnParts>, SocketAddr)> {
        let mut last_res = None;
        for addr in addrs {
            let res = self.timed_connect(stream_type, addr, timeout).await;
            match res {
                Ok(res) => {
                    last_res = Some(Ok((res, addr)));
                    break;
                }
                Err(e) => last_res = Some(Err(e)),
            }
        }
        last_res.unwrap_or_else(|| Err(io::Error::other("no addrs")))
    }

    pub async fn timed_connect(
        &self,
        stream_type: &str,
        addr: SocketAddr,
        timeout: Duration,
    ) -> io::Result<Box<dyn ConnParts>> {
        let Some(connector) = self.connectors.get(stream_type) else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("invalid stream type: `{stream_type}`"),
            ));
        };
        StreamConnectExt::timed_connect(connector.deref(), addr, timeout).await
    }
    pub fn reset_addr(&self, stream_type: &str, addr: SocketAddr) {
        if let Some(connector) = self.connectors.get(stream_type) {
            connector.reset_addr(addr);
        }
    }
    pub fn reoptimize(&self, stream_type: &str, addr: SocketAddr) {
        if let Some(connector) = self.connectors.get(stream_type) {
            connector.reoptimize(addr);
        }
    }
    pub fn session_stats(&self, stream_type: &str, addr: SocketAddr) -> Option<String> {
        self.connectors.get(stream_type)?.session_stats(addr)
    }
    pub fn reports_session_stats(&self, stream_type: &str) -> bool {
        self.connectors
            .get(stream_type)
            .is_some_and(|connector| connector.reports_session_stats())
    }
    pub fn register_named(
        &self,
        stream_type: Arc<str>,
        name: Arc<str>,
        connector: Arc<dyn NamedStreamConnect>,
    ) -> NamedStreamRegistration {
        self.named.register(stream_type, name, connector)
    }
    pub async fn timed_connect_named(
        &self,
        stream_type: &str,
        name: &str,
        timeout: Duration,
    ) -> io::Result<Box<dyn ConnParts>> {
        tokio::time::timeout(timeout, self.named.connect(stream_type, name))
            .await
            .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "timed out"))?
    }
    pub fn named_session_stats(&self, stream_type: &str, name: &str) -> Option<String> {
        self.named.session_stats(stream_type, name)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::connect::ConnectorConfig;
    use crate::stream::{HasIoAddr, OwnIoStream};
    use std::pin::Pin;
    use std::task::{Context, Poll};
    use tokio::io::{AsyncRead, AsyncWrite, DuplexStream, ReadBuf};

    const STREAM_TYPE: &str = "test";

    #[derive(Debug)]
    struct TestConn {
        io: DuplexStream,
        addr: SocketAddr,
    }

    impl AsyncRead for TestConn {
        fn poll_read(
            mut self: Pin<&mut Self>,
            cx: &mut Context<'_>,
            buf: &mut ReadBuf<'_>,
        ) -> Poll<io::Result<()>> {
            Pin::new(&mut self.io).poll_read(cx, buf)
        }
    }

    impl AsyncWrite for TestConn {
        fn poll_write(
            mut self: Pin<&mut Self>,
            cx: &mut Context<'_>,
            buf: &[u8],
        ) -> Poll<io::Result<usize>> {
            Pin::new(&mut self.io).poll_write(cx, buf)
        }
        fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            Pin::new(&mut self.io).poll_flush(cx)
        }
        fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            Pin::new(&mut self.io).poll_shutdown(cx)
        }
    }

    impl HasIoAddr for TestConn {
        fn peer_addr(&self) -> io::Result<SocketAddr> {
            Ok(self.addr)
        }
        fn local_addr(&self) -> io::Result<SocketAddr> {
            Ok(self.addr)
        }
    }

    impl OwnIoStream for TestConn {}
    impl ConnParts for TestConn {}

    #[derive(Debug)]
    struct FallbackConnector {
        attempted: std::sync::Mutex<Vec<SocketAddr>>,
        successful: SocketAddr,
    }

    #[derive(Debug)]
    struct NamedConnector {
        addr: SocketAddr,
    }
    #[async_trait]
    impl NamedStreamConnect for NamedConnector {
        async fn connect(&self) -> io::Result<Box<dyn ConnParts>> {
            let (io, _peer) = tokio::io::duplex(1);
            Ok(Box::new(TestConn {
                io,
                addr: self.addr,
            }))
        }
    }

    fn empty_table() -> StreamConnectorTable {
        StreamConnectorTable::new(
            crate::connect::connector_config_cell(ConnectorConfig::default()).0,
            HashMap::new(),
        )
    }

    #[async_trait]
    impl StreamConnect for FallbackConnector {
        async fn connect(&self, addr: SocketAddr) -> io::Result<Box<dyn ConnParts>> {
            self.attempted.lock().unwrap().push(addr);
            if addr != self.successful {
                return Err(io::Error::from(io::ErrorKind::NetworkUnreachable));
            }
            let (io, _peer) = tokio::io::duplex(1);
            Ok(Box::new(TestConn { io, addr }))
        }
    }

    #[tokio::test]
    async fn tries_next_resolved_address_after_non_refused_error() {
        let unreachable = "[2001:db8::1]:443".parse().unwrap();
        let successful = "192.0.2.1:443".parse().unwrap();
        let connector = Arc::new(FallbackConnector {
            attempted: std::sync::Mutex::new(Vec::new()),
            successful,
        });
        let table = StreamConnectorTable::new(
            crate::connect::connector_config_cell(ConnectorConfig::default()).0,
            HashMap::from([(
                Arc::from(STREAM_TYPE),
                connector.clone() as Arc<dyn StreamConnect>,
            )]),
        );
        let (_, connected_addr) = table
            .timed_connect_any(
                STREAM_TYPE,
                [unreachable, successful],
                Duration::from_secs(1),
            )
            .await
            .unwrap();
        assert_eq!(connected_addr, successful);
        assert_eq!(
            *connector.attempted.lock().unwrap(),
            [unreachable, successful]
        );
    }

    #[tokio::test]
    async fn named_connect_waits_for_registration() {
        let table = Arc::new(empty_table());
        let mut tasks = tokio::task::JoinSet::new();
        tasks.spawn({
            let table = Arc::clone(&table);
            async move {
                table
                    .timed_connect_named(STREAM_TYPE, "private-a", Duration::from_secs(1))
                    .await
                    .unwrap()
                    .peer_addr()
                    .unwrap()
            }
        });
        tokio::task::yield_now().await;
        let expected = "192.0.2.10:443".parse().unwrap();
        let _registration = table.register_named(
            STREAM_TYPE.into(),
            "private-a".into(),
            Arc::new(NamedConnector { addr: expected }),
        );
        assert_eq!(tasks.join_next().await.unwrap().unwrap(), expected);
    }

    #[tokio::test]
    async fn stale_registration_cannot_remove_its_replacement() {
        let table = empty_table();
        let old = table.register_named(
            STREAM_TYPE.into(),
            "private-a".into(),
            Arc::new(NamedConnector {
                addr: "192.0.2.1:443".parse().unwrap(),
            }),
        );
        let expected = "192.0.2.2:443".parse().unwrap();
        let _replacement = table.register_named(
            STREAM_TYPE.into(),
            "private-a".into(),
            Arc::new(NamedConnector { addr: expected }),
        );
        drop(old);
        let connected = table
            .timed_connect_named(STREAM_TYPE, "private-a", Duration::from_secs(1))
            .await
            .unwrap();
        assert_eq!(connected.peer_addr().unwrap(), expected);
    }
}
