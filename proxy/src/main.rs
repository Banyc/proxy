use std::net::SocketAddr;

use get_config::toml::get_config;
use serde::Deserialize;
use server::tcp_proxy::TcpProxy;
use tokio::net::TcpListener;

#[tokio::main]
pub async fn main() {
    let config: Config = get_config().unwrap();
    let tcp_listener = TcpListener::bind(config.listen_addr).await.unwrap();
    let tcp_proxy = TcpProxy::new(tcp_listener);
    tcp_proxy.serve().await.unwrap();
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Deserialize)]
struct Config {
    listen_addr: SocketAddr,
}
