use std::net::SocketAddr;

use get_config::toml::get_config;
use serde::Deserialize;
use server::{tcp_proxy::TcpProxy, udp_proxy::UdpProxy};
use tokio::net::{TcpListener, UdpSocket};
use tracing_subscriber::EnvFilter;

#[tokio::main]
pub async fn main() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .with_line_number(true)
        .try_init();
    let config: Config = get_config().unwrap();
    let mut join_set = tokio::task::JoinSet::new();
    join_set.spawn(async move {
        let tcp_listener = TcpListener::bind(config.listen_addr).await.unwrap();
        let tcp_proxy = TcpProxy::new(tcp_listener);
        tcp_proxy.serve().await.unwrap();
    });
    join_set.spawn(async move {
        let udp_listener = UdpSocket::bind(config.listen_addr).await.unwrap();
        let udp_proxy = UdpProxy::new(udp_listener);
        udp_proxy.serve().await.unwrap();
    });
    join_set.join_next().await.unwrap().unwrap();
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Deserialize)]
struct Config {
    listen_addr: SocketAddr,
}
