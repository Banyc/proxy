#![feature(result_option_inspect)]

use common::error::AnyResult;
use serde::Deserialize;
use stream::{kcp::KcpProxyServerBuilder, tcp::TcpProxyServerBuilder};
use udp::UdpProxyServerBuilder;

pub mod stream;
pub mod udp;

#[derive(Debug, Clone, PartialEq, Eq, Hash, Deserialize)]
pub struct ProxyServerSpawner {
    pub tcp_server: Option<Vec<TcpProxyServerBuilder>>,
    pub udp_server: Option<Vec<UdpProxyServerBuilder>>,
    pub kcp_server: Option<Vec<KcpProxyServerBuilder>>,
}

impl ProxyServerSpawner {
    pub async fn spawn(self, join_set: &mut tokio::task::JoinSet<AnyResult>) {
        if let Some(tcp_server) = self.tcp_server {
            for server in tcp_server {
                join_set.spawn(async move {
                    let server = server.build().await?;
                    server.serve().await?;
                    Ok(())
                });
            }
        }
        if let Some(udp_server) = self.udp_server {
            for server in udp_server {
                join_set.spawn(async move {
                    let server = server.build().await?;
                    server.serve().await?;
                    Ok(())
                });
            }
        }
        if let Some(kcp_server) = self.kcp_server {
            for server in kcp_server {
                join_set.spawn(async move {
                    let server = server.build().await?;
                    server.serve().await?;
                    Ok(())
                });
            }
        }
    }
}
