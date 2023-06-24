use std::{collections::HashMap, io, sync::Arc};

use common::{error::AnyResult, filter::FilterBuilder, loading, stream::pool::PoolBuilder};
use serde::{Deserialize, Serialize};
use stream::{
    proxy_table::StreamProxyTableBuilder,
    streams::{
        http_tunnel::{HttpAccess, HttpAccessServerConfig},
        tcp::{TcpAccess, TcpAccessServerConfig},
    },
};
use udp::{proxy_table::UdpProxyTableBuilder, UdpAccess, UdpAccessServerConfig};

pub mod stream;
pub mod udp;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AccessServerConfig {
    pub tcp_server: Option<Vec<TcpAccessServerConfig>>,
    pub udp_server: Option<Vec<UdpAccessServerConfig>>,
    pub http_server: Option<Vec<HttpAccessServerConfig>>,
    pub stream_pool: PoolBuilder,
    pub stream_proxy_tables: Option<HashMap<Arc<str>, StreamProxyTableBuilder>>,
    pub udp_proxy_tables: Option<HashMap<Arc<str>, UdpProxyTableBuilder>>,
    pub filters: Option<HashMap<Arc<str>, FilterBuilder>>,
}

impl AccessServerConfig {
    pub fn new() -> AccessServerConfig {
        AccessServerConfig {
            tcp_server: None,
            udp_server: None,
            http_server: None,
            stream_pool: PoolBuilder(None),
            stream_proxy_tables: None,
            udp_proxy_tables: None,
            filters: None,
        }
    }

    pub async fn spawn_and_kill(
        self,
        join_set: &mut tokio::task::JoinSet<AnyResult>,
        loader: &mut AccessServerLoader,
    ) -> io::Result<()> {
        // Shared
        let stream_pool = self.stream_pool.build();
        let stream_proxy_tables = self.stream_proxy_tables.unwrap_or_default();
        let stream_proxy_tables = stream_proxy_tables
            .into_iter()
            .map(|(k, v)| (k, v.build(&stream_pool)))
            .collect::<HashMap<_, _>>();
        let udp_proxy_tables = self.udp_proxy_tables.unwrap_or_default();
        let udp_proxy_tables = udp_proxy_tables
            .into_iter()
            .map(|(k, v)| (k, v.build()))
            .collect::<HashMap<_, _>>();
        let filters = self.filters.unwrap_or_default();
        let filters = filters
            .into_iter()
            .map(|(k, v)| (k, v.build().unwrap()))
            .collect::<HashMap<_, _>>();

        // TCP servers
        let tcp_server = self.tcp_server.unwrap_or_default();
        let tcp_server = tcp_server
            .into_iter()
            .map(|c| c.into_builder(stream_pool.clone(), &stream_proxy_tables))
            .collect::<Vec<_>>();
        loader.tcp_server.load(join_set, tcp_server).await?;

        // UDP servers
        let udp_server = self.udp_server.unwrap_or_default();
        let udp_server = udp_server
            .into_iter()
            .map(|c| c.into_builder(&udp_proxy_tables))
            .collect::<Vec<_>>();
        loader.udp_server.load(join_set, udp_server).await?;

        // HTTP servers
        let http_server = self.http_server.unwrap_or_default();
        let http_server = http_server
            .into_iter()
            .map(|c| c.into_builder(stream_pool.clone(), &stream_proxy_tables, &filters))
            .collect::<Vec<_>>();
        loader.http_server.load(join_set, http_server).await?;

        Ok(())
    }
}

impl Default for AccessServerConfig {
    fn default() -> Self {
        Self::new()
    }
}

pub struct AccessServerLoader {
    tcp_server: loading::Loader<TcpAccess>,
    udp_server: loading::Loader<UdpAccess>,
    http_server: loading::Loader<HttpAccess>,
}

impl AccessServerLoader {
    pub fn new() -> Self {
        Self {
            tcp_server: loading::Loader::new(),
            udp_server: loading::Loader::new(),
            http_server: loading::Loader::new(),
        }
    }
}

impl Default for AccessServerLoader {
    fn default() -> Self {
        Self::new()
    }
}
