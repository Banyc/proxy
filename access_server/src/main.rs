use access_server::{
    http_::HttpProxyAccessBuilder, tcp::TcpProxyAccessBuilder, udp::UdpProxyAccessBuilder,
};
use get_config::toml::get_config;
use serde::{Deserialize, Serialize};

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt::init();
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
    for http_server in config.http_servers {
        join_set.spawn(async move {
            let server = http_server.build().await.unwrap();
            server.serve().await.unwrap();
        });
    }
    join_set.join_next().await.unwrap().unwrap();
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    pub tcp_servers: Vec<TcpProxyAccessBuilder>,
    pub udp_servers: Vec<UdpProxyAccessBuilder>,
    pub http_servers: Vec<HttpProxyAccessBuilder>,
}
