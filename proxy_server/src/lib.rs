#![feature(result_option_inspect)]

use std::io;

use common::{error::AnyResult, loading};
use serde::Deserialize;
use stream::{kcp::KcpProxyServerBuilder, tcp::TcpProxyServerBuilder, StreamProxyServer};
use udp::{UdpProxyServer, UdpProxyServerBuilder};

pub mod stream;
pub mod udp;

#[derive(Debug, Clone, PartialEq, Eq, Hash, Deserialize)]
pub struct ProxyServerSpawner {
    pub tcp_server: Option<Vec<TcpProxyServerBuilder>>,
    pub udp_server: Option<Vec<UdpProxyServerBuilder>>,
    pub kcp_server: Option<Vec<KcpProxyServerBuilder>>,
}

impl ProxyServerSpawner {
    pub async fn spawn(
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

pub struct ProxyServerLoader {
    tcp_server: loading::Loader<StreamProxyServer>,
    udp_server: loading::Loader<UdpProxyServer>,
    kcp_server: loading::Loader<StreamProxyServer>,
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
