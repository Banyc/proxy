use std::io;

use common::{error::AnyResult, loading};
use serde::Deserialize;
use stream::{kcp::KcpProxyServerBuilder, tcp::TcpProxyServerBuilder, StreamProxy};
use thiserror::Error;
use udp::{UdpProxy, UdpProxyServerBuilder};

pub mod stream;
pub mod udp;

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProxyServerConfig {
    #[serde(default)]
    pub tcp_server: Vec<TcpProxyServerBuilder>,
    #[serde(default)]
    pub udp_server: Vec<UdpProxyServerBuilder>,
    #[serde(default)]
    pub kcp_server: Vec<KcpProxyServerBuilder>,
}

impl ProxyServerConfig {
    pub fn new() -> Self {
        Self {
            tcp_server: Default::default(),
            udp_server: Default::default(),
            kcp_server: Default::default(),
        }
    }

    pub async fn spawn_and_kill(
        self,
        join_set: &mut tokio::task::JoinSet<AnyResult>,
        loader: &mut ProxyServerLoader,
    ) -> AnyResult {
        loader.tcp_server.load(join_set, self.tcp_server).await?;

        loader.udp_server.load(join_set, self.udp_server).await?;

        loader.kcp_server.load(join_set, self.kcp_server).await?;

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
}

impl ProxyServerLoader {
    pub fn new() -> Self {
        Self {
            tcp_server: loading::Loader::new(),
            udp_server: loading::Loader::new(),
            kcp_server: loading::Loader::new(),
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
