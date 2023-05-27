#![feature(result_option_inspect)]

use common::error::AnyResult;
use serde::Deserialize;
use stream::{kcp::KcpProxyServerBuilder, tcp::TcpProxyServerBuilder};
use udp::UdpProxyServerBuilder;

pub mod stream;
pub mod udp;

#[derive(Debug, Clone, PartialEq, Eq, Hash, Deserialize)]
pub struct ProxyServerSpawner {
    pub tcp_servers: Option<Vec<TcpProxyServerBuilder>>,
    pub udp_servers: Option<Vec<UdpProxyServerBuilder>>,
    pub kcp_servers: Option<Vec<KcpProxyServerBuilder>>,
}

impl ProxyServerSpawner {
    pub async fn spawn(self, join_set: &mut tokio::task::JoinSet<AnyResult>) {
        if let Some(tcp_servers) = self.tcp_servers {
            for tcp_server in tcp_servers {
                join_set.spawn(async move {
                    let server = tcp_server.build().await?;
                    server.serve().await?;
                    Ok(())
                });
            }
        }
        if let Some(udp_servers) = self.udp_servers {
            for udp_server in udp_servers {
                join_set.spawn(async move {
                    let server = udp_server.build().await?;
                    server.serve().await?;
                    Ok(())
                });
            }
        }
        if let Some(kcp_servers) = self.kcp_servers {
            for kcp_server in kcp_servers {
                join_set.spawn(async move {
                    let server = kcp_server.build().await?;
                    server.serve().await?;
                    Ok(())
                });
            }
        }
    }
}
