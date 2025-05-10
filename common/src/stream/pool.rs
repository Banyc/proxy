use std::{
    collections::HashMap, convert::Infallible, io, marker::PhantomData, net::SocketAddr, sync::Arc,
    time::Duration,
};

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio_conn_pool::{ConnPool, ConnPoolEntry};

use crate::{
    config::{Merge, SharableConfig},
    header::heartbeat::send_noop,
    proxy_table::{IntoAddr, ProxyConfig, ProxyConfigBuildError, ProxyConfigBuilder},
    stream::HasIoAddr,
};

use super::{
    OwnIoStream,
    addr::{AsStreamType, StreamAddr},
    connect::StreamTypedConnect,
    context::StreamContext,
};

const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(30);

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[serde(bound(deserialize = "AddrStr: Deserialize<'de>"))]
pub struct PoolBuilder<AddrStr>(
    #[serde(default)] pub Vec<SharableConfig<ProxyConfigBuilder<AddrStr>>>,
);
impl<AddrStr> PoolBuilder<AddrStr> {
    pub fn new() -> Self {
        Self(vec![])
    }
}
impl<AddrStr, StreamType> PoolBuilder<AddrStr>
where
    AddrStr: IntoAddr<Addr = StreamAddr<StreamType>>,
{
    pub fn build<Conn, ConnectorTable>(
        self,
        connector_table: Arc<ConnectorTable>,
        proxy_server: &HashMap<Arc<str>, ProxyConfig<StreamAddr<StreamType>>>,
    ) -> Result<ConnPool<StreamAddr<StreamType>, Conn>, PoolBuildError>
    where
        StreamType: AsStreamType + Clone,
        Conn: std::fmt::Debug + OwnIoStream,
        ConnectorTable: StreamTypedConnect<Conn = Conn, StreamType = StreamType>,
    {
        let c = self
            .0
            .into_iter()
            .map(|c| match c {
                SharableConfig::SharingKey(k) => proxy_server
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
    ProxyConfigBuild(#[from] ProxyConfigBuildError),
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

fn pool_entries_from_proxy_configs<Conn, ConnectorTable, StreamType>(
    proxy_configs: impl Iterator<Item = ProxyConfig<StreamAddr<StreamType>>>,
    connector_table: Arc<ConnectorTable>,
) -> impl Iterator<Item = ConnPoolEntry<StreamAddr<StreamType>, Conn>>
where
    Conn: std::fmt::Debug + OwnIoStream,
    ConnectorTable:
        StreamTypedConnect<Conn = Conn, StreamType = StreamType> + Sync + Send + 'static,
    StreamType: AsStreamType,
{
    proxy_configs.map(move |c| ConnPoolEntry {
        key: c.address.clone(),
        connect: Arc::new(PoolConnector {
            proxy_server: c.clone(),
            connector_table: connector_table.clone(),
        }),
        heartbeat: Arc::new(PoolHeartbeat {
            proxy_server: c.clone(),
            connection: Default::default(),
        }),
    })
}

#[derive(Debug)]
struct PoolConnector<StreamType, ConnectorTable> {
    proxy_server: ProxyConfig<StreamAddr<StreamType>>,
    connector_table: Arc<ConnectorTable>,
}
#[async_trait]
impl<Conn, ConnectorTable, StreamType> tokio_conn_pool::Connect
    for PoolConnector<StreamType, ConnectorTable>
where
    Conn: OwnIoStream,
    ConnectorTable:
        StreamTypedConnect<Conn = Conn, StreamType = StreamType> + Sync + Send + 'static,
    StreamType: AsStreamType,
{
    type Connection = Conn;
    async fn connect(&self) -> Option<Self::Connection> {
        let addr = self.proxy_server.address.clone();
        let sock_addr = addr.address.to_socket_addr().await.ok()?;
        self.connector_table
            .timed_connect(
                &self.proxy_server.address.stream_type,
                sock_addr,
                HEARTBEAT_INTERVAL,
            )
            .await
            .ok()
    }
}

#[derive(Debug)]
struct PoolHeartbeat<Conn, StreamType> {
    proxy_server: ProxyConfig<StreamAddr<StreamType>>,
    connection: PhantomData<Conn>,
}
#[async_trait]
impl<Conn, StreamType> tokio_conn_pool::Heartbeat for PoolHeartbeat<Conn, StreamType>
where
    Conn: OwnIoStream,
    StreamType: AsStreamType,
{
    type Connection = Conn;
    async fn heartbeat(&self, mut conn: Self::Connection) -> Option<Self::Connection> {
        send_noop(
            &mut conn,
            HEARTBEAT_INTERVAL,
            &self.proxy_server.header_crypto.clone(),
        )
        .await
        .ok()?;
        Some(conn)
    }
}

pub async fn connect_with_pool<Conn, ConnectorTable, StreamType>(
    addr: &StreamAddr<StreamType>,
    stream_context: &StreamContext<Conn, ConnectorTable, StreamType>,
    allow_loopback: bool,
    timeout: Duration,
) -> Result<(Conn, SocketAddr), ConnectError<StreamType>>
where
    Conn: HasIoAddr + Sync + Send + 'static,
    ConnectorTable: StreamTypedConnect<Conn = Conn, StreamType = StreamType>,
    StreamType: AsStreamType,
{
    let stream = stream_context.pool.inner().pull(addr);
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
    let stream = stream_context
        .connector_table
        .timed_connect(&addr.stream_type, sock_addr, timeout)
        .await
        .map_err(|e| ConnectError::ConnectAddr {
            source: e,
            addr: addr.clone(),
            sock_addr,
        })?;
    Ok((stream, sock_addr))
}
#[derive(Debug, Error)]
pub enum ConnectError<StreamType: std::fmt::Display> {
    #[error("Failed to resolve address: {source}, {addr}")]
    ResolveAddr {
        #[source]
        source: io::Error,
        addr: StreamAddr<StreamType>,
    },
    #[error("Refused to connect to loopback address: {addr}, {sock_addr}")]
    Loopback {
        addr: StreamAddr<StreamType>,
        sock_addr: SocketAddr,
    },
    #[error("Failed to connect to address: {source}, {addr}, {sock_addr}")]
    ConnectAddr {
        #[source]
        source: io::Error,
        addr: StreamAddr<StreamType>,
        sock_addr: SocketAddr,
    },
}
