use std::net::SocketAddr;

use common::crypto::XorCrypto;
use get_config::toml::get_config;
use proxy_server::{tcp_proxy::TcpProxy, udp_proxy::UdpProxy};
use serde::Deserialize;
use tracing_subscriber::EnvFilter;

#[tokio::main]
pub async fn main() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .with_line_number(true)
        .try_init();
    let config: Config = get_config().unwrap();
    let mut join_set = tokio::task::JoinSet::new();
    join_set.spawn({
        let header_crypto = XorCrypto::new(config.header_xor_key.clone());
        let payload_crypto = config.payload_xor_key.clone().map(XorCrypto::new);
        async move {
            let tcp_proxy = TcpProxy::new(header_crypto, payload_crypto);
            let server = tcp_proxy.build(config.listen_addr).await.unwrap();
            server.serve().await.unwrap();
        }
    });
    join_set.spawn(async move {
        let crypto = XorCrypto::new(config.header_xor_key);
        let udp_proxy = UdpProxy::new(crypto);
        let server = udp_proxy.build(config.listen_addr).await.unwrap();
        server.serve().await.unwrap();
    });
    join_set.join_next().await.unwrap().unwrap();
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Deserialize)]
struct Config {
    listen_addr: SocketAddr,
    header_xor_key: Vec<u8>,
    payload_xor_key: Option<Vec<u8>>,
}
