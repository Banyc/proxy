use std::net::SocketAddr;

use access::tcp::TcpProxyAccess;
use common::header::{ProxyConfig, XorCrypto};
use get_config::toml::get_config;
use serde::{Deserialize, Serialize};

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt::init();
    let config: Config = get_config().unwrap();
    let mut join_set = tokio::task::JoinSet::new();
    join_set.spawn({
        async move {
            let tcp_access = TcpProxyAccess::new(
                config
                    .proxy_configs
                    .into_iter()
                    .map(|x| x.build())
                    .collect(),
                config.destination,
            );
            let server = tcp_access.build(config.listen_addr).await.unwrap();
            server.serve().await.unwrap();
        }
    });
    join_set.join_next().await.unwrap().unwrap();
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
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
