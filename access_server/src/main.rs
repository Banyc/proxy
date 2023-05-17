use access_server::{http_::HttpProxyAccess, tcp::TcpProxyAccess, udp::UdpProxyAccess};
use common::{crypto::XorCrypto, header::ProxyConfig};
use get_config::toml::get_config;
use serde::{Deserialize, Serialize};

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt::init();
    let config: Config = get_config().unwrap();
    let Config {
        tcp: tcp_config,
        udp: udp_config,
        http: http_config,
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
                config.destination.into(),
                config.payload_xor_key.map(XorCrypto::new),
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
                config.destination.into(),
            );
            let server = access.build(config.listen_addr).await.unwrap();
            server.serve().await.unwrap();
        });
    }
    if let Some(config) = http_config {
        join_set.spawn(async move {
            let access = HttpProxyAccess::new(
                config
                    .proxy_configs
                    .into_iter()
                    .map(|x| x.build())
                    .collect(),
                config.payload_xor_key.map(XorCrypto::new),
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
    http: Option<HttpConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TransportConfig {
    listen_addr: String,
    proxy_configs: Vec<ProxyConfigBuilder>,
    destination: String,
    payload_xor_key: Option<Vec<u8>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HttpConfig {
    listen_addr: String,
    proxy_configs: Vec<ProxyConfigBuilder>,
    payload_xor_key: Option<Vec<u8>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProxyConfigBuilder {
    pub address: String,
    pub xor_key: Vec<u8>,
}

impl ProxyConfigBuilder {
    pub fn build(self) -> ProxyConfig {
        ProxyConfig {
            address: self.address.into(),
            crypto: XorCrypto::new(self.xor_key),
        }
    }
}
