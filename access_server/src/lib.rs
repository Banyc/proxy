#![feature(result_option_inspect)]

use common::error::AnyResult;
use http_tunnel::HttpProxyAccessBuilder;
use serde::{Deserialize, Serialize};
use tcp::TcpProxyAccessBuilder;
use udp::UdpProxyAccessBuilder;

pub mod http_tunnel;
pub mod tcp;
pub mod udp;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AccessServerSpawner {
    pub tcp_server: Option<Vec<TcpProxyAccessBuilder>>,
    pub udp_server: Option<Vec<UdpProxyAccessBuilder>>,
    pub http_server: Option<Vec<HttpProxyAccessBuilder>>,
}

impl AccessServerSpawner {
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
        if let Some(http_server) = self.http_server {
            for server in http_server {
                join_set.spawn(async move {
                    let server = server.build().await?;
                    server.serve().await?;
                    Ok(())
                });
            }
        }
    }
}
