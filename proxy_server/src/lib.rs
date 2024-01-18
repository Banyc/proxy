use std::io;

use common::{
    error::AnyResult,
    loading,
    session_table::BothSessionTables,
    stream::concrete::{addr::ConcreteStreamType, pool::ConcreteConnPool},
};
use serde::Deserialize;
use stream::{
    kcp::KcpProxyServerConfig, mptcp::MptcpProxyServerConfig, tcp::TcpProxyServerConfig,
    StreamProxy,
};
use thiserror::Error;
use udp::{UdpProxy, UdpProxyServerBuilder, UdpProxyServerConfig};

pub mod stream;
pub mod udp;

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProxyServerConfig {
    #[serde(default)]
    pub tcp_server: Vec<TcpProxyServerConfig>,
    #[serde(default)]
    pub udp_server: Vec<UdpProxyServerConfig>,
    #[serde(default)]
    pub kcp_server: Vec<KcpProxyServerConfig>,
    #[serde(default)]
    pub mptcp_server: Vec<MptcpProxyServerConfig>,
}

impl ProxyServerConfig {
    pub fn new() -> Self {
        Self {
            tcp_server: Default::default(),
            udp_server: Default::default(),
            kcp_server: Default::default(),
            mptcp_server: Default::default(),
        }
    }

    pub async fn load_and_clean(
        self,
        join_set: &mut tokio::task::JoinSet<AnyResult>,
        loader: &mut ProxyServerLoader,
        stream_pool: &ConcreteConnPool,
        session_table: &BothSessionTables<ConcreteStreamType>,
    ) -> AnyResult {
        loader
            .tcp_server
            .load_and_clean(
                join_set,
                self.tcp_server
                    .into_iter()
                    .map(|s| s.into_builder(stream_pool.clone(), session_table.stream().cloned()))
                    .collect(),
            )
            .await?;

        loader
            .udp_server
            .load_and_clean(
                join_set,
                self.udp_server
                    .into_iter()
                    .map(|config| UdpProxyServerBuilder {
                        config,
                        session_table: session_table.udp().cloned(),
                    })
                    .collect(),
            )
            .await?;

        loader
            .kcp_server
            .load_and_clean(
                join_set,
                self.kcp_server
                    .into_iter()
                    .map(|s| s.into_builder(stream_pool.clone(), session_table.stream().cloned()))
                    .collect(),
            )
            .await?;

        loader
            .mptcp_server
            .load_and_clean(
                join_set,
                self.mptcp_server
                    .into_iter()
                    .map(|s| s.into_builder(stream_pool.clone(), session_table.stream().cloned()))
                    .collect(),
            )
            .await?;

        Ok(())
    }
}

impl Default for ProxyServerConfig {
    fn default() -> Self {
        Self::new()
    }
}

pub struct ProxyServerLoader {
    tcp_server: loading::Loader<StreamProxy>,
    udp_server: loading::Loader<UdpProxy>,
    kcp_server: loading::Loader<StreamProxy>,
    mptcp_server: loading::Loader<StreamProxy>,
}

impl ProxyServerLoader {
    pub fn new() -> Self {
        Self {
            tcp_server: loading::Loader::new(),
            udp_server: loading::Loader::new(),
            kcp_server: loading::Loader::new(),
            mptcp_server: loading::Loader::new(),
        }
    }
}

impl Default for ProxyServerLoader {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug, Error)]
#[error("Failed to bind to listen address: {0}")]
pub struct ListenerBindError(#[source] io::Error);
