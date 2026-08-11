use std::{convert::Infallible, io, net::SocketAddr, sync::Arc, time::Duration};

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio_conn_pool::{ConnPool, ConnPoolEntry};

use crate::{
    config::{Merge, SharableConfig},
    header::preamble::send_keep_alive,
    proto::{addr::RouteAddr, connect::stream::StreamConnectorTable, context::StreamRuntime},
    route::{ConnConfig, ConnConfigBuildError, Registries},
};

use super::ConnParts;

const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(30);

pub type StreamPoolBuilder = PoolBuilder;
pub type StreamConnPool = ConnPool<RouteAddr, Box<dyn ConnParts>>;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PoolBuilder(#[serde(default)] pub Vec<SharableConfig<ConnConfig>>);
impl PoolBuilder {
    pub fn new() -> Self {
        Self(vec![])
    }
    pub fn resolve(
        self,
        registries: &Registries<'_>,
    ) -> Result<ConnPool<RouteAddr, Box<dyn ConnParts>>, PoolBuildError> {
        let c = self
            .0
            .into_iter()
            .map(|c| match c {
                SharableConfig::SharingKey(k) => registries
                    .conn
                    .get(&k)
                    .cloned()
                    .ok_or(PoolBuildError::ProxyServerKeyNotFound(k)),
                SharableConfig::Private(c) => Ok(c),
            })
            .collect::<Result<Vec<_>, _>>()?;
        let entries =
            pool_entries_from_proxy_configs(c.into_iter(), registries.connector_table.clone());
        let pool = ConnPool::new(entries);
        Ok(pool)
    }
}
#[derive(Debug, Error)]
pub enum PoolBuildError {
    #[error("{0}")]
    ProxyConfigBuild(#[from] ConnConfigBuildError),
    #[error("Proxy server key not found: {0}")]
    ProxyServerKeyNotFound(Arc<str>),
}
impl Default for PoolBuilder {
    fn default() -> Self {
        Self::new()
    }
}
impl Merge for PoolBuilder {
    type Error = Infallible;

    fn merge(mut self, other: Self) -> Result<Self, Self::Error>
    where
        Self: Sized,
    {
        self.0.extend(other.0);
        Ok(Self(self.0))
    }
}

fn pool_entries_from_proxy_configs(
    proxy_configs: impl Iterator<Item = ConnConfig>,
    connector_table: Arc<StreamConnectorTable>,
) -> impl Iterator<Item = ConnPoolEntry<RouteAddr, Box<dyn ConnParts>>> {
    proxy_configs.map(move |c| ConnPoolEntry {
        key: c.address.clone(),
        connect: Arc::new(PoolConnector {
            conn: c.clone(),
            connector_table: connector_table.clone(),
        }),
        heartbeat: Arc::new(PoolHeartbeat { conn: c.clone() }),
    })
}

#[derive(Debug)]
struct PoolConnector {
    conn: ConnConfig,
    connector_table: Arc<StreamConnectorTable>,
}
#[async_trait]
impl tokio_conn_pool::Connect for PoolConnector {
    type Connection = Box<dyn ConnParts>;
    async fn connect(&self) -> Option<Self::Connection> {
        let addr = self.conn.address.clone();
        if let Some((_, name)) = addr.reverse_tunnel() {
            return self
                .connector_table
                .timed_connect_named(&addr.protocol, name, HEARTBEAT_INTERVAL)
                .await
                .ok();
        }
        let sock_addrs = addr.address.to_socket_addrs().await.ok()?;
        let (stream, _sock_addr) = self
            .connector_table
            .timed_connect_any(&self.conn.address.protocol, sock_addrs, HEARTBEAT_INTERVAL)
            .await
            .ok()?;
        Some(stream)
    }
}

#[derive(Debug)]
struct PoolHeartbeat {
    conn: ConnConfig,
}
#[async_trait]
impl tokio_conn_pool::Heartbeat for PoolHeartbeat {
    type Connection = Box<dyn ConnParts>;
    async fn heartbeat(&self, mut conn: Self::Connection) -> Option<Self::Connection> {
        send_keep_alive(
            &mut conn,
            HEARTBEAT_INTERVAL,
            &self.conn.header_crypto.clone(),
        )
        .await
        .ok()?;
        Some(conn)
    }
}

pub async fn connect_with_pool(
    addr: &RouteAddr,
    stream_context: &StreamRuntime,
    allow_loopback: bool,
    timeout: Duration,
) -> Result<(Box<dyn ConnParts>, SocketAddr), ConnectError> {
    let stream = stream_context.pool.inner().pull(addr);
    let sock_addr = stream.as_ref().and_then(|s| s.peer_addr().ok());
    if let (Some(stream), Some(sock_addr)) = (stream, sock_addr) {
        return Ok((stream, sock_addr));
    }
    if let Some((_, name)) = addr.reverse_tunnel() {
        let stream = stream_context
            .connector_table
            .timed_connect_named(&addr.protocol, name, timeout)
            .await
            .map_err(|source| ConnectError::ConnectNamed {
                source,
                addr: addr.clone(),
            })?;
        let sock_addr = stream
            .peer_addr()
            .map_err(|source| ConnectError::ConnectNamed {
                source,
                addr: addr.clone(),
            })?;
        return Ok((stream, sock_addr));
    }
    let sock_addrs =
        addr.address
            .to_socket_addrs()
            .await
            .map_err(|e| ConnectError::ResolveAddr {
                source: e,
                addr: addr.clone(),
            })?;
    if !allow_loopback
        && sock_addrs
            .iter()
            .any(|addr| crate::addr::reaches_loopback(&addr.ip()))
    {
        return Err(ConnectError::Loopback {
            addr: addr.clone(),
            sock_addrs: sock_addrs.into(),
        });
    }
    let (stream, sock_addr) = stream_context
        .connector_table
        .timed_connect_any(&addr.protocol, sock_addrs.iter().copied(), timeout)
        .await
        .map_err(|e| ConnectError::ConnectAddr {
            source: e,
            addr: addr.clone(),
            sock_addrs: sock_addrs.into(),
        })?;
    Ok((stream, sock_addr))
}
#[derive(Debug, Error)]
pub enum ConnectError {
    #[error("Failed to resolve address: {source}, {addr}")]
    ResolveAddr {
        #[source]
        source: io::Error,
        addr: RouteAddr,
    },
    #[error("Refused to connect to loopback address: {addr}, {sock_addrs:?}")]
    Loopback {
        addr: RouteAddr,
        sock_addrs: Vec<SocketAddr>,
    },
    #[error("Failed to connect to address: {source}, {addr}, {sock_addrs:?}")]
    ConnectAddr {
        #[source]
        source: io::Error,
        addr: RouteAddr,
        sock_addrs: Vec<SocketAddr>,
    },
    #[error("Failed to connect to named stream: {source}, {addr}")]
    ConnectNamed {
        #[source]
        source: io::Error,
        addr: RouteAddr,
    },
}

#[cfg(test)]
mod tests {
    use std::{
        collections::HashMap,
        pin::Pin,
        task::{Context, Poll},
    };

    use super::*;
    use crate::{
        addr::InternetAddrKind,
        connect::{ConnectorConfig, connector_config_cell},
        stream::{HasIoAddr, OwnIoStream},
    };
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
    struct TestConnect {
        addr: SocketAddr,
    }
    #[async_trait]
    impl tokio_conn_pool::Connect for TestConnect {
        type Connection = Box<dyn ConnParts>;
        async fn connect(&self) -> Option<Self::Connection> {
            let (io, _peer) = tokio::io::duplex(1);
            Some(Box::new(TestConn {
                io,
                addr: self.addr,
            }))
        }
    }

    #[derive(Debug)]
    struct TestHeartbeat;
    #[async_trait]
    impl tokio_conn_pool::Heartbeat for TestHeartbeat {
        type Connection = Box<dyn ConnParts>;
        async fn heartbeat(&self, conn: Self::Connection) -> Option<Self::Connection> {
            Some(conn)
        }
    }

    fn stream_addr(addr: &str, protocol: &str) -> RouteAddr {
        RouteAddr {
            address: addr.parse::<SocketAddr>().unwrap().into(),
            protocol: protocol.into(),
        }
    }

    fn conn_config(addr: &str, protocol: &str) -> ConnConfig {
        ConnConfig {
            address: stream_addr(addr, protocol),
            header_crypto: tokio_chacha20::config::Config::new(
                [7; tokio_chacha20::KEY_BYTES].into(),
            ),
            payload_crypto: None,
        }
    }

    fn entry(key: &RouteAddr) -> ConnPoolEntry<RouteAddr, Box<dyn ConnParts>> {
        ConnPoolEntry {
            key: key.clone(),
            connect: Arc::new(TestConnect {
                addr: socket_addr_of(key),
            }),
            heartbeat: Arc::new(TestHeartbeat),
        }
    }

    fn empty_table() -> Arc<StreamConnectorTable> {
        Arc::new(StreamConnectorTable::new(
            connector_config_cell(ConnectorConfig::default()).0,
            HashMap::new(),
        ))
    }

    async fn pull_until_ready(
        pool: &ConnPool<RouteAddr, Box<dyn ConnParts>>,
        key: &RouteAddr,
    ) -> Box<dyn ConnParts> {
        for _ in 0..10_000 {
            if let Some(conn) = pool.pull(key) {
                return conn;
            }
            tokio::task::yield_now().await;
        }
        panic!("the pool never produced a connection for {key}");
    }

    fn socket_addr_of(key: &RouteAddr) -> SocketAddr {
        match *key.address {
            InternetAddrKind::SocketAddr(addr) => addr,
            InternetAddrKind::DomainName { .. } => panic!("test keys are socket addrs"),
        }
    }

    #[test]
    fn entries_are_keyed_by_the_config_address_and_stream_type() {
        let entries: Vec<_> = pool_entries_from_proxy_configs(
            [
                conn_config("127.0.0.1:1", "tcp"),
                conn_config("127.0.0.1:2", "tcp"),
                conn_config("127.0.0.1:1", "udp"),
            ]
            .into_iter(),
            empty_table(),
        )
        .collect();
        assert_eq!(entries.len(), 3);
        assert_eq!(entries[0].key, stream_addr("127.0.0.1:1", "tcp"));
        assert_eq!(entries[1].key, stream_addr("127.0.0.1:2", "tcp"));
        assert_ne!(entries[0].key, entries[2].key);
        assert_eq!(entries[2].key, stream_addr("127.0.0.1:1", "udp"));
    }

    #[test]
    fn duplicate_configs_collapse_onto_one_key() {
        let entries: Vec<_> = pool_entries_from_proxy_configs(
            [
                conn_config("127.0.0.1:1", "tcp"),
                conn_config("127.0.0.1:1", "tcp"),
            ]
            .into_iter(),
            empty_table(),
        )
        .collect();
        assert_eq!(entries[0].key, entries[1].key);
        assert_eq!(entries[0].key, stream_addr("127.0.0.1:1", "tcp"));
    }

    #[test]
    fn an_unknown_sharing_key_is_rejected_at_build() {
        use crate::route::{ProbeRtt, Registries};
        use tokio_util::sync::CancellationToken;
        struct NoTracer;
        impl ProbeRtt for NoTracer {
            fn probe_rtt(
                &self,
                _chain: &crate::route::ConnChain,
            ) -> std::pin::Pin<
                Box<dyn std::future::Future<Output = crate::route::ProbeOutcome> + Send>,
            > {
                unreachable!()
            }
        }
        let conn: HashMap<Arc<str>, ConnConfig> = HashMap::new();
        let matcher = Arc::new(HashMap::new());
        let conn_selector = HashMap::new();
        let tracer: Arc<dyn ProbeRtt + Send + Sync> = Arc::new(NoTracer);
        let registries = Registries {
            conn: &conn,
            matcher: &matcher,
            conn_selector: &conn_selector,
            tracer: &tracer,
            connector_table: &empty_table(),
            cancellation: CancellationToken::new(),
        };
        let err = PoolBuilder(vec![SharableConfig::SharingKey("missing".into())])
            .resolve(&registries)
            .unwrap_err();
        assert!(
            matches!(err, PoolBuildError::ProxyServerKeyNotFound(ref k) if k.as_ref() == "missing"),
            "{err:?}"
        );
    }

    #[tokio::test]
    async fn an_unconfigured_key_is_not_in_the_pool() {
        let key = stream_addr("127.0.0.1:1", STREAM_TYPE);
        let pool = ConnPool::new([entry(&key)].into_iter());
        let other = stream_addr("127.0.0.1:2", STREAM_TYPE);
        assert!(pool.pull(&other).is_none());
    }

    #[tokio::test]
    async fn connections_are_pulled_back_by_key() {
        let a_tcp = stream_addr("127.0.0.1:1", "tcp");
        let b_tcp = stream_addr("127.0.0.1:2", "tcp");
        let a_udp = stream_addr("127.0.0.1:1", "udp");
        let pool = ConnPool::new([entry(&a_tcp), entry(&b_tcp), entry(&a_udp)].into_iter());

        let conn = pull_until_ready(&pool, &a_tcp).await;
        assert_eq!(conn.peer_addr().unwrap(), socket_addr_of(&a_tcp));
        let conn = pull_until_ready(&pool, &b_tcp).await;
        assert_eq!(conn.peer_addr().unwrap(), socket_addr_of(&b_tcp));
        let conn = pull_until_ready(&pool, &a_udp).await;
        assert_eq!(conn.peer_addr().unwrap(), socket_addr_of(&a_udp));
    }
}
