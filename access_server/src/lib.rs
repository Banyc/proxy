#![feature(result_option_inspect)]

use http_tunnel::HttpProxyAccessBuilder;
use serde::{Deserialize, Serialize};
use tcp::TcpProxyAccessBuilder;
use udp::UdpProxyAccessBuilder;

pub mod http_tunnel;
pub mod tcp;
pub mod udp;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AccessServerSpawner {
    pub tcp_servers: Option<Vec<TcpProxyAccessBuilder>>,
    pub udp_servers: Option<Vec<UdpProxyAccessBuilder>>,
    pub http_servers: Option<Vec<HttpProxyAccessBuilder>>,
}

impl AccessServerSpawner {
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
        if let Some(http_servers) = self.http_servers {
            for http_server in http_servers {
                join_set.spawn(async move {
                    let server = http_server.build().await.unwrap();
                    server.serve().await.unwrap();
                });
            }
        }
    }
}
