use std::{io, marker::PhantomData, net::SocketAddr, sync::Arc, time::Duration};

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
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
    addr::{StreamAddr, StreamAddrStr, StreamType},
    connect::StreamConnectorTable,
    context::StreamContext,
    IoStream,
};

const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(30);

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[serde(bound(deserialize = "SAS: Deserialize<'de>"))]
pub struct PoolBuilder<SAS>(#[serde(default)] pub Vec<StreamProxyConfigBuilder<SAS>>);
impl<SAS> PoolBuilder<SAS> {
    pub fn new() -> Self {
        Self(vec![])
    }
}
impl<SAS, ST> PoolBuilder<SAS>
where
    SAS: StreamAddrStr<StreamType = ST>,
    ST: StreamType,
{
    pub fn build<C, CT>(
        self,
        connector_table: CT,
    ) -> Result<ConnPool<StreamAddr<ST>, C>, StreamProxyConfigBuildError>
    where
        C: std::fmt::Debug + IoStream,
        CT: StreamConnectorTable<Connection = C, StreamType = ST>,
    {
        let c = self
            .0
            .into_iter()
            .map(|c| c.build::<ST>())
            .collect::<Result<Vec<_>, _>>()?;
        let entries = pool_entries_from_proxy_configs(c.into_iter(), connector_table.clone());
        let pool = ConnPool::new(entries);
        Ok(pool)
    }
}
impl<SAS> Default for PoolBuilder<SAS> {
    fn default() -> Self {
        Self::new()
    }
}

fn pool_entries_from_proxy_configs<C, CT: Clone, ST>(
    proxy_configs: impl Iterator<Item = ProxyConfig<StreamAddr<ST>>>,
    connector_table: CT,
) -> impl Iterator<Item = ConnPoolEntry<StreamAddr<ST>, C>>
where
    C: std::fmt::Debug + IoStream,
    CT: StreamConnectorTable<Connection = C, StreamType = ST> + Sync + Send + 'static,
    ST: StreamType,
{
    proxy_configs.map(move |c| ConnPoolEntry {
        key: c.address.clone(),
        connect: Arc::new(PoolConnector {
            proxy_config: c.clone(),
            connector_table: connector_table.clone(),
        }),
        heartbeat: Arc::new(PoolHeartbeat {
            proxy_config: c.clone(),
            connection: Default::default(),
        }),
    })
}

#[derive(Debug)]
struct PoolConnector<ST, CT> {
    proxy_config: ProxyConfig<StreamAddr<ST>>,
    connector_table: CT,
}
#[async_trait]
impl<C, CT, ST> tokio_conn_pool::Connect for PoolConnector<ST, CT>
where
    C: IoStream,
    CT: StreamConnectorTable<Connection = C, StreamType = ST> + Sync + Send + 'static,
    ST: StreamType,
{
    type Connection = C;
    async fn connect(&self) -> Option<Self::Connection> {
        let addr = self.proxy_config.address.clone();
        let sock_addr = addr.address.to_socket_addr().await.ok()?;
        self.connector_table
            .timed_connect(
                &self.proxy_config.address.stream_type,
                sock_addr,
                HEARTBEAT_INTERVAL,
            )
            .await
            .ok()
    }
}

#[derive(Debug)]
struct PoolHeartbeat<C, ST> {
    proxy_config: ProxyConfig<StreamAddr<ST>>,
    connection: PhantomData<C>,
}
#[async_trait]
impl<C, ST> tokio_conn_pool::Heartbeat for PoolHeartbeat<C, ST>
where
    C: IoStream,
    ST: StreamType,
{
    type Connection = C;
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

pub async fn connect_with_pool<C, CT, ST>(
    addr: &StreamAddr<ST>,
    stream_context: &StreamContext<C, CT, ST>,
    allow_loopback: bool,
    timeout: Duration,
) -> Result<(C, SocketAddr), ConnectError<ST>>
where
    C: IoAddr + Sync + Send + 'static,
    CT: StreamConnectorTable<Connection = C, StreamType = ST>,
    ST: StreamType,
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
pub enum ConnectError<ST: std::fmt::Display> {
    #[error("Failed to resolve address: {source}, {addr}")]
    ResolveAddr {
        #[source]
        source: io::Error,
        addr: StreamAddr<ST>,
    },
    #[error("Refused to connect to loopback address: {addr}, {sock_addr}")]
    Loopback {
        addr: StreamAddr<ST>,
        sock_addr: SocketAddr,
    },
    #[error("Failed to connect to address: {source}, {addr}, {sock_addr}")]
    ConnectAddr {
        #[source]
        source: io::Error,
        addr: StreamAddr<ST>,
        sock_addr: SocketAddr,
    },
}
