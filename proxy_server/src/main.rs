use get_config::toml::get_config;
use proxy_server::{
    tcp_proxy_server::TcpProxyServerBuilder, udp_proxy_server::UdpProxyServerBuilder,
};
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
    for tcp_server in config.tcp_servers {
        join_set.spawn(async move {
            let server = tcp_server.build().await.unwrap();
            server.serve().await.unwrap();
        });
    }
    for udp_server in config.udp_servers {
        join_set.spawn(async move {
            let server = udp_server.build().await.unwrap();
            server.serve().await.unwrap();
        });
    }
    join_set.join_next().await.unwrap().unwrap();
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Deserialize)]
pub struct Config {
    pub tcp_servers: Vec<TcpProxyServerBuilder>,
    pub udp_servers: Vec<UdpProxyServerBuilder>,
}
