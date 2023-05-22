use access_server::AccessServerSpawner;
use get_config::toml::get_config;
use proxy_server::ProxyServerSpawner;
use serde::Deserialize;

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt::init();
    let config: ServerConfig = get_config().unwrap();
    let mut join_set = tokio::task::JoinSet::new();
    if let Some(access_server) = config.access_server {
        access_server.spawn(&mut join_set).await;
    }
    if let Some(proxy_server) = config.proxy_server {
        proxy_server.spawn(&mut join_set).await;
    }
    join_set.join_next().await.unwrap().unwrap();
}

#[derive(Debug, Deserialize)]
pub struct ServerConfig {
    pub access_server: Option<AccessServerSpawner>,
    pub proxy_server: Option<ProxyServerSpawner>,
}
