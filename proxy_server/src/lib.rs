#![feature(result_option_inspect)]

use serde::Deserialize;
use tcp_proxy_server::TcpProxyServerBuilder;
use udp_proxy_server::UdpProxyServerBuilder;

pub mod tcp_proxy_server;
pub mod udp_proxy_server;

#[derive(Debug, Clone, PartialEq, Eq, Hash, Deserialize)]
pub struct ProxyServerSpawner {
    pub tcp_servers: Option<Vec<TcpProxyServerBuilder>>,
    pub udp_servers: Option<Vec<UdpProxyServerBuilder>>,
}

impl ProxyServerSpawner {
    pub async fn spawn(self, join_set: &mut tokio::task::JoinSet<()>) {
        if let Some(tcp_servers) = self.tcp_servers {
            for tcp_server in tcp_servers {
                join_set.spawn(async move {
                    let server = tcp_server.build().await.unwrap();
                    server.serve().await.unwrap();
                });
            }
        }
        if let Some(udp_servers) = self.udp_servers {
            for udp_server in udp_servers {
                join_set.spawn(async move {
                    let server = udp_server.build().await.unwrap();
                    server.serve().await.unwrap();
                });
            }
        }
    }
}
