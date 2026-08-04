use std::{
    collections::HashMap, convert::Infallible, io, net::SocketAddr, sync::Arc, time::Duration,
};

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio_conn_pool::{ConnPool, ConnPoolEntry};

use crate::{
    config::{Merge, SharableConfig},
    header::heartbeat::send_noop,
    proto::{
        addr::{StreamAddr, StreamAddrStr},
        connect::stream::StreamConnectorTable,
        context::StreamContext,
    },
    route::{ConnConfig, ConnConfigBuildError, ConnConfigBuilder, IntoAddr},
};

use super::AsConn;

const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(30);

pub type StreamPoolBuilder = PoolBuilder<StreamAddrStr>;
pub type StreamConnPool = ConnPool<StreamAddr, Box<dyn AsConn>>;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[serde(bound(deserialize = "AddrStr: Deserialize<'de>"))]
pub struct PoolBuilder<AddrStr>(
    #[serde(default)] pub Vec<SharableConfig<ConnConfigBuilder<AddrStr>>>,
);
impl<AddrStr> PoolBuilder<AddrStr> {
    pub fn new() -> Self {
        Self(vec![])
    }
}
impl<AddrStr> PoolBuilder<AddrStr>
where
    AddrStr: IntoAddr<Addr = StreamAddr>,
{
    pub fn build(
        self,
        connector_table: Arc<StreamConnectorTable>,
        conn: &HashMap<Arc<str>, ConnConfig<StreamAddr>>,
    ) -> Result<ConnPool<StreamAddr, Box<dyn AsConn>>, PoolBuildError> {
        let c = self
            .0
            .into_iter()
            .map(|c| match c {
                SharableConfig::SharingKey(k) => conn
                    .get(&k)
                    .cloned()
                    .ok_or(PoolBuildError::ProxyServerKeyNotFound(k)),
                SharableConfig::Private(c) => c.build().map_err(PoolBuildError::ProxyConfigBuild),
            })
            .collect::<Result<Vec<_>, _>>()?;
        let entries = pool_entries_from_proxy_configs(c.into_iter(), connector_table.clone());
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
impl<AddrStr> Default for PoolBuilder<AddrStr> {
    fn default() -> Self {
        Self::new()
    }
}
impl<AddrStr> Merge for PoolBuilder<AddrStr> {
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
    proxy_configs: impl Iterator<Item = ConnConfig<StreamAddr>>,
    connector_table: Arc<StreamConnectorTable>,
) -> impl Iterator<Item = ConnPoolEntry<StreamAddr, Box<dyn AsConn>>> {
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
    conn: ConnConfig<StreamAddr>,
    connector_table: Arc<StreamConnectorTable>,
}
#[async_trait]
impl tokio_conn_pool::Connect for PoolConnector {
    type Connection = Box<dyn AsConn>;
    async fn connect(&self) -> Option<Self::Connection> {
        let addr = self.conn.address.clone();
        let sock_addrs = addr.address.to_socket_addrs().await.ok()?;
        let (stream, _sock_addr) = self
            .connector_table
            .timed_connect_2(
                &self.conn.address.stream_type,
                sock_addrs,
                HEARTBEAT_INTERVAL,
            )
            .await
            .ok()?;
        Some(stream)
    }
}

#[derive(Debug)]
struct PoolHeartbeat {
    conn: ConnConfig<StreamAddr>,
}
#[async_trait]
impl tokio_conn_pool::Heartbeat for PoolHeartbeat {
    type Connection = Box<dyn AsConn>;
    async fn heartbeat(&self, mut conn: Self::Connection) -> Option<Self::Connection> {
        send_noop(
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
    addr: &StreamAddr,
    stream_context: &StreamContext,
    allow_loopback: bool,
    timeout: Duration,
) -> Result<(Box<dyn AsConn>, SocketAddr), ConnectError> {
    let stream = stream_context.pool.inner().pull(addr);
    let sock_addr = stream.as_ref().and_then(|s| s.peer_addr().ok());
    if let (Some(stream), Some(sock_addr)) = (stream, sock_addr) {
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
        .timed_connect_2(&addr.stream_type, sock_addrs.iter().copied(), timeout)
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
        addr: StreamAddr,
    },
    #[error("Refused to connect to loopback address: {addr}, {sock_addrs:?}")]
    Loopback {
        addr: StreamAddr,
        sock_addrs: Vec<SocketAddr>,
    },
    #[error("Failed to connect to address: {source}, {addr}, {sock_addrs:?}")]
    ConnectAddr {
        #[source]
        source: io::Error,
        addr: StreamAddr,
        sock_addrs: Vec<SocketAddr>,
    },
}

#[cfg(test)]
mod tests {
    use std::{
        pin::Pin,
        sync::RwLock,
        task::{Context, Poll},
    };

    use super::*;
    use crate::{
        addr::InternetAddrKind,
        connect::ConnectorConfig,
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
    impl AsConn for TestConn {}

    #[derive(Debug)]
    struct TestConnect {
        addr: SocketAddr,
    }
    #[async_trait]
    impl tokio_conn_pool::Connect for TestConnect {
        type Connection = Box<dyn AsConn>;
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
        type Connection = Box<dyn AsConn>;
        async fn heartbeat(&self, conn: Self::Connection) -> Option<Self::Connection> {
            Some(conn)
        }
    }

    fn stream_addr(addr: &str, stream_type: &str) -> StreamAddr {
        StreamAddr {
            address: addr.parse::<SocketAddr>().unwrap().into(),
            stream_type: stream_type.into(),
        }
    }

    fn conn_config(addr: &str, stream_type: &str) -> ConnConfig<StreamAddr> {
        ConnConfig {
            address: stream_addr(addr, stream_type),
            header_crypto: tokio_chacha20::config::Config::new(
                [7; tokio_chacha20::KEY_BYTES].into(),
            ),
            payload_crypto: None,
        }
    }

    fn entry(key: &StreamAddr) -> ConnPoolEntry<StreamAddr, Box<dyn AsConn>> {
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
            Arc::new(RwLock::new(ConnectorConfig::default())),
            HashMap::new(),
        ))
    }

    async fn pull_until_ready(
        pool: &ConnPool<StreamAddr, Box<dyn AsConn>>,
        key: &StreamAddr,
    ) -> Box<dyn AsConn> {
        for _ in 0..10_000 {
            if let Some(conn) = pool.pull(key) {
                return conn;
            }
            tokio::task::yield_now().await;
        }
        panic!("the pool never produced a connection for {key}");
    }

    fn socket_addr_of(key: &StreamAddr) -> SocketAddr {
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
        // The same address with a different stream type is a distinct key.
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
        let err = PoolBuilder::<StreamAddrStr>(vec![SharableConfig::SharingKey("missing".into())])
            .build(empty_table(), &HashMap::new())
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
