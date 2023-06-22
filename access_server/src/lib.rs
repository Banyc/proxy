use std::io;

use common::{error::AnyResult, loading};
use http_tunnel::{HttpProxyAccess, HttpProxyAccessBuilder};
use serde::{Deserialize, Serialize};
use tcp::{TcpProxyAccess, TcpProxyAccessBuilder};
use udp::{UdpProxyAccess, UdpProxyAccessBuilder};

pub mod http_tunnel;
pub mod tcp;
pub mod udp;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AccessServerConfig {
    pub tcp_server: Option<Vec<TcpProxyAccessBuilder>>,
    pub udp_server: Option<Vec<UdpProxyAccessBuilder>>,
    pub http_server: Option<Vec<HttpProxyAccessBuilder>>,
}

impl AccessServerConfig {
    pub fn new() -> AccessServerConfig {
        AccessServerConfig {
            tcp_server: None,
            udp_server: None,
            http_server: None,
        }
    }

    pub async fn spawn_and_kill(
        self,
        join_set: &mut tokio::task::JoinSet<AnyResult>,
        loader: &mut AccessServerLoader,
    ) -> io::Result<()> {
        let tcp_server = self.tcp_server.unwrap_or_default();
        loader.tcp_server.load(join_set, tcp_server).await?;

        let udp_server = self.udp_server.unwrap_or_default();
        loader.udp_server.load(join_set, udp_server).await?;

        let http_server = self.http_server.unwrap_or_default();
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
    tcp_server: loading::Loader<TcpProxyAccess>,
    udp_server: loading::Loader<UdpProxyAccess>,
    http_server: loading::Loader<HttpProxyAccess>,
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
