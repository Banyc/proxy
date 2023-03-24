use std::net::SocketAddr;

use access::{tcp::TcpProxyAccess, udp::UdpProxyAccess};
use common::header::{ProxyConfig, XorCrypto};
use get_config::toml::get_config;
use serde::{Deserialize, Serialize};

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt::init();
    let config: Config = get_config().unwrap();
    let Config {
        tcp: tcp_config,
        udp: udp_config,
    } = config;
    let mut join_set = tokio::task::JoinSet::new();
    if let Some(config) = tcp_config {
        join_set.spawn(async move {
            let access = TcpProxyAccess::new(
                config
                    .proxy_configs
                    .into_iter()
                    .map(|x| x.build())
                    .collect(),
                config.destination,
            );
            let server = access.build(config.listen_addr).await.unwrap();
            server.serve().await.unwrap();
        });
    }
    if let Some(config) = udp_config {
        join_set.spawn(async move {
            let access = UdpProxyAccess::new(
                config
                    .proxy_configs
                    .into_iter()
                    .map(|x| x.build())
                    .collect(),
                config.destination,
            );
            let server = access.build(config.listen_addr).await.unwrap();
            server.serve().await.unwrap();
        });
    }
    join_set.join_next().await.unwrap().unwrap();
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    tcp: Option<TransportConfig>,
    udp: Option<TransportConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TransportConfig {
    listen_addr: SocketAddr,
    proxy_configs: Vec<ProxyConfigBuilder>,
    destination: SocketAddr,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProxyConfigBuilder {
    pub address: SocketAddr,
    pub xor_key: Vec<u8>,
}

impl ProxyConfigBuilder {
    pub fn build(self) -> ProxyConfig {
        ProxyConfig {
            address: self.address,
            crypto: XorCrypto::new(self.xor_key),
        }
    }
}
