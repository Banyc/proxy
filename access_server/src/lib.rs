use std::{collections::HashMap, io, sync::Arc};

use common::{
    error::AnyResult,
    filter::{self, FilterBuilder},
    loading,
    stream::pool::PoolBuilder,
};
use serde::{Deserialize, Serialize};
use socks5::server::{
    tcp::{Socks5ServerTcpAccess, Socks5ServerTcpAccessServerConfig},
    udp::{Socks5ServerUdpAccess, Socks5ServerUdpAccessServerConfig},
};
use stream::{
    proxy_table::StreamProxyTableBuilder,
    streams::{
        http_tunnel::{HttpAccess, HttpAccessServerConfig},
        tcp::{TcpAccess, TcpAccessServerConfig},
    },
};
use udp::{proxy_table::UdpProxyTableBuilder, UdpAccess, UdpAccessServerConfig};

pub mod socks5;
pub mod stream;
pub mod udp;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AccessServerConfig {
    #[serde(default)]
    pub tcp_server: Vec<TcpAccessServerConfig>,
    #[serde(default)]
    pub udp_server: Vec<UdpAccessServerConfig>,
    #[serde(default)]
    pub http_server: Vec<HttpAccessServerConfig>,
    #[serde(default)]
    pub socks5_tcp_server: Vec<Socks5ServerTcpAccessServerConfig>,
    #[serde(default)]
    pub socks5_udp_server: Vec<Socks5ServerUdpAccessServerConfig>,
    pub stream_pool: PoolBuilder,
    #[serde(default)]
    pub stream_proxy_tables: HashMap<Arc<str>, StreamProxyTableBuilder>,
    #[serde(default)]
    pub udp_proxy_tables: HashMap<Arc<str>, UdpProxyTableBuilder>,
    #[serde(default)]
    pub filters: HashMap<Arc<str>, FilterBuilder>,
}

impl AccessServerConfig {
    pub fn new() -> AccessServerConfig {
        AccessServerConfig {
            tcp_server: Default::default(),
            udp_server: Default::default(),
            http_server: Default::default(),
            socks5_tcp_server: Default::default(),
            socks5_udp_server: Default::default(),
            stream_pool: PoolBuilder(None),
            stream_proxy_tables: Default::default(),
            udp_proxy_tables: Default::default(),
            filters: Default::default(),
        }
    }

    pub async fn spawn_and_kill(
        self,
        join_set: &mut tokio::task::JoinSet<AnyResult>,
        loader: &mut AccessServerLoader,
    ) -> io::Result<()> {
        // Shared
        let stream_pool = self.stream_pool.build();
        let stream_proxy_tables = self
            .stream_proxy_tables
            .into_iter()
            .map(|(k, v)| match v.build(&stream_pool) {
                Ok(v) => Ok((k, v)),
                Err(e) => Err(e),
            })
            .collect::<Result<HashMap<_, _>, _>>()
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;
        let udp_proxy_tables = self
            .udp_proxy_tables
            .into_iter()
            .map(|(k, v)| match v.build() {
                Ok(v) => Ok((k, v)),
                Err(e) => Err(e),
            })
            .collect::<Result<HashMap<_, _>, _>>()
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;
        let filters = filter::build_from_map(self.filters)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;

        // TCP servers
        let tcp_server = self
            .tcp_server
            .into_iter()
            .map(|c| c.into_builder(stream_pool.clone(), &stream_proxy_tables))
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;
        loader.tcp_server.load(join_set, tcp_server).await?;

        // UDP servers
        let udp_server = self
            .udp_server
            .into_iter()
            .map(|c| c.into_builder(&udp_proxy_tables))
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;
        loader.udp_server.load(join_set, udp_server).await?;

        // HTTP servers
        let http_server = self
            .http_server
            .into_iter()
            .map(|c| c.into_builder(stream_pool.clone(), &stream_proxy_tables, &filters))
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;
        loader.http_server.load(join_set, http_server).await?;

        // SOCKS5 TCP servers
        let socks5_tcp_server = self
            .socks5_tcp_server
            .into_iter()
            .map(|c| c.into_builder(stream_pool.clone(), &stream_proxy_tables, &filters))
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;
        loader
            .socks5_tcp_server
            .load(join_set, socks5_tcp_server)
            .await?;

        // SOCKS5 UDP servers
        let socks5_udp_server = self
            .socks5_udp_server
            .into_iter()
            .map(|c| c.into_builder(&udp_proxy_tables))
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;
        loader
            .socks5_udp_server
            .load(join_set, socks5_udp_server)
            .await?;

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
    socks5_tcp_server: loading::Loader<Socks5ServerTcpAccess>,
    socks5_udp_server: loading::Loader<Socks5ServerUdpAccess>,
}

impl AccessServerLoader {
    pub fn new() -> Self {
        Self {
            tcp_server: loading::Loader::new(),
            udp_server: loading::Loader::new(),
            http_server: loading::Loader::new(),
            socks5_tcp_server: loading::Loader::new(),
            socks5_udp_server: loading::Loader::new(),
        }
    }
}

impl Default for AccessServerLoader {
    fn default() -> Self {
        Self::new()
    }
}
