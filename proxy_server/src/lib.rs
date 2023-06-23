use std::io;

use common::{error::AnyResult, loading};
use serde::Deserialize;
use stream::{kcp::KcpProxyServerBuilder, tcp::TcpProxyServerBuilder, StreamProxy};
use udp::{UdpProxy, UdpProxyServerBuilder};

pub mod stream;
pub mod udp;

#[derive(Debug, Clone, PartialEq, Eq, Hash, Deserialize)]
pub struct ProxyServerConfig {
    pub tcp_server: Option<Vec<TcpProxyServerBuilder>>,
    pub udp_server: Option<Vec<UdpProxyServerBuilder>>,
    pub kcp_server: Option<Vec<KcpProxyServerBuilder>>,
}

impl ProxyServerConfig {
    pub fn new() -> Self {
        Self {
            tcp_server: None,
            udp_server: None,
            kcp_server: None,
        }
    }

    pub async fn spawn_and_kill(
        self,
        join_set: &mut tokio::task::JoinSet<AnyResult>,
        loader: &mut ProxyServerLoader,
    ) -> io::Result<()> {
        let tcp_server = self.tcp_server.unwrap_or_default();
        loader.tcp_server.load(join_set, tcp_server).await?;

        let udp_server = self.udp_server.unwrap_or_default();
        loader.udp_server.load(join_set, udp_server).await?;

        let kcp_server = self.kcp_server.unwrap_or_default();
        loader.kcp_server.load(join_set, kcp_server).await?;

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
