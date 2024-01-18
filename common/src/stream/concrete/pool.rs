use std::{io, net::SocketAddr, sync::Arc, time::Duration};

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use swap::Swap;
use thiserror::Error;
use tokio_conn_pool::{ConnPool, ConnPoolEntry};

use crate::{
    header::heartbeat::send_noop,
    proxy_table::ProxyConfig,
    stream::{
        proxy_table::{StreamProxyConfigBuildError, StreamProxyConfigBuilder},
        IoAddr,
    },
};

use super::{
    addr::{ConcreteStreamAddr, ConcreteStreamAddrStr},
    connector_table::STREAM_CONNECTOR_TABLE,
    created_stream::CreatedStream,
};

const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(30);

pub type ConcreteConnPool = ConnPool<ConcreteStreamAddr, CreatedStream>;
pub type SharedConcreteConnPool = Swap<ConcreteConnPool>;
type ConcreteConnPoolEntry = ConnPoolEntry<ConcreteStreamAddr, CreatedStream>;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PoolBuilder(#[serde(default)] pub Vec<StreamProxyConfigBuilder<ConcreteStreamAddrStr>>);
impl PoolBuilder {
    pub fn new() -> Self {
        Self(vec![])
    }

    pub fn build(self) -> Result<ConcreteConnPool, StreamProxyConfigBuildError> {
        let c = self
            .0
            .into_iter()
            .map(|c| c.build())
            .collect::<Result<Vec<_>, _>>()?;
        let entries = pool_entries_from_proxy_configs(c.into_iter());
        let pool = ConnPool::new(entries);
        Ok(pool)
    }
}
impl Default for PoolBuilder {
    fn default() -> Self {
        Self::new()
    }
}

fn pool_entries_from_proxy_configs(
    proxy_configs: impl Iterator<Item = ProxyConfig<ConcreteStreamAddr>>,
) -> impl Iterator<Item = ConcreteConnPoolEntry> {
    proxy_configs.map(|c| ConcreteConnPoolEntry {
        key: c.address.clone(),
        connect: Arc::new(PoolConnector {
            proxy_config: c.clone(),
        }),
        heartbeat: Arc::new(PoolHeartbeat {
            proxy_config: c.clone(),
        }),
    })
}

#[derive(Debug)]
struct PoolConnector {
    proxy_config: ProxyConfig<ConcreteStreamAddr>,
}
#[async_trait]
impl tokio_conn_pool::Connect for PoolConnector {
    type Connection = CreatedStream;
    async fn connect(&self) -> Option<Self::Connection> {
        let addr = self.proxy_config.address.clone();
        let sock_addr = addr.address.to_socket_addr().await.ok()?;
        STREAM_CONNECTOR_TABLE
            .timed_connect(
                self.proxy_config.address.stream_type,
                sock_addr,
                HEARTBEAT_INTERVAL,
            )
            .await
            .ok()
    }
}

#[derive(Debug)]
struct PoolHeartbeat {
    proxy_config: ProxyConfig<ConcreteStreamAddr>,
}
#[async_trait]
impl tokio_conn_pool::Heartbeat for PoolHeartbeat {
    type Connection = CreatedStream;
    async fn heartbeat(&self, mut conn: Self::Connection) -> Option<Self::Connection> {
        send_noop(
            &mut conn,
            HEARTBEAT_INTERVAL,
            &self.proxy_config.crypto.clone(),
        )
        .await
        .ok()?;
        Some(conn)
    }
}

pub async fn connect_with_pool(
    addr: &ConcreteStreamAddr,
    stream_pool: &SharedConcreteConnPool,
    allow_loopback: bool,
    timeout: Duration,
) -> Result<(CreatedStream, SocketAddr), ConnectError> {
    let stream = stream_pool.inner().pull(addr);
    let sock_addr = stream.as_ref().and_then(|s| s.peer_addr().ok());
    if let (Some(stream), Some(sock_addr)) = (stream, sock_addr) {
        return Ok((stream, sock_addr));
    }

    let sock_addr = addr
        .address
        .to_socket_addr()
        .await
        .map_err(|e| ConnectError::ResolveAddr {
            source: e,
            addr: addr.clone(),
        })?;
    if !allow_loopback && sock_addr.ip().is_loopback() {
        // Prevent connections to localhost
        return Err(ConnectError::Loopback {
            addr: addr.clone(),
            sock_addr,
        });
    }
    let stream = STREAM_CONNECTOR_TABLE
        .timed_connect(addr.stream_type, sock_addr, timeout)
        .await
        .map_err(|e| ConnectError::ConnectAddr {
            source: e,
            addr: addr.clone(),
            sock_addr,
        })?;
    Ok((stream, sock_addr))
}

#[derive(Debug, Error)]
pub enum ConnectError {
    #[error("Failed to resolve address: {source}, {addr}")]
    ResolveAddr {
        #[source]
        source: io::Error,
        addr: ConcreteStreamAddr,
    },
    #[error("Refused to connect to loopback address: {addr}, {sock_addr}")]
    Loopback {
        addr: ConcreteStreamAddr,
        sock_addr: SocketAddr,
    },
    #[error("Failed to connect to address: {source}, {addr}, {sock_addr}")]
    ConnectAddr {
        #[source]
        source: io::Error,
        addr: ConcreteStreamAddr,
        sock_addr: SocketAddr,
    },
}

#[cfg(test)]
mod tests {
    use tokio::{io::AsyncReadExt, net::TcpListener, task::JoinSet};

    use crate::stream::{addr::StreamAddr, concrete::addr::ConcreteStreamType};

    use super::*;

    async fn spawn_listener() -> SocketAddr {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            loop {
                let (mut stream, _) = listener.accept().await.unwrap();
                tokio::spawn(async move {
                    let mut buf = [0; 1024];
                    loop {
                        if let Err(_e) = stream.read_exact(&mut buf).await {
                            break;
                        }
                    }
                });
            }
        });
        addr
    }

    fn create_random_crypto() -> tokio_chacha20::config::Config {
        let key: [u8; 32] = rand::random();
        tokio_chacha20::config::Config::new(key.into())
    }

    #[tokio::test]
    async fn take_none() {
        let addr = "0.0.0.0:0".parse::<SocketAddr>().unwrap();
        let addr = StreamAddr {
            address: addr.into(),
            stream_type: ConcreteStreamType::Tcp,
        };
        let proxy_config = ProxyConfig {
            address: addr.clone(),
            crypto: create_random_crypto(),
        };
        let entries = pool_entries_from_proxy_configs([proxy_config].into_iter());
        let pool = Swap::new(ConnPool::<ConcreteStreamAddr, CreatedStream>::new(entries));
        let mut join_set = JoinSet::new();
        for _ in 0..100 {
            let pool = pool.clone();
            let addr = addr.clone();
            join_set.spawn(async move {
                let res = pool.inner().pull(&addr);
                assert!(res.is_none());
            });
        }
    }

    #[tokio::test]
    async fn take_some() {
        let addr = spawn_listener().await;
        let addr = StreamAddr {
            address: addr.into(),
            stream_type: ConcreteStreamType::Tcp,
        };
        let proxy_config = ProxyConfig {
            address: addr.clone(),
            crypto: create_random_crypto(),
        };
        let entries = pool_entries_from_proxy_configs([proxy_config].into_iter());
        let pool = ConnPool::<ConcreteStreamAddr, CreatedStream>::new(entries);
        for _ in 0..10 {
            tokio::time::sleep(Duration::from_millis(500)).await;
            for _ in 0..1 {
                let addr = addr.clone();
                let res = pool.pull(&addr);
                assert!(res.is_some());
            }
        }
    }
}
