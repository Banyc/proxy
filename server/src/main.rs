use access_server::AccessServerSpawner;
use clap::Parser;
use proxy_server::ProxyServerSpawner;
use serde::Deserialize;

#[derive(Debug, Parser)]
struct Args {
    /// Paths to the configuration files.
    config_files: Vec<String>,
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt::init();
    let args = Args::parse();
    let mut config_str = String::new();
    for config_file in args.config_files {
        config_str.push_str(&std::fs::read_to_string(config_file).unwrap());
    }
    let config: ServerConfig = toml::from_str(&config_str).unwrap();
    let mut join_set = tokio::task::JoinSet::new();
    if let Some(access_server) = config.access_server {
        access_server.spawn(&mut join_set).await;
    }
    if let Some(proxy_server) = config.proxy_server {
        proxy_server.spawn(&mut join_set).await;
    }
    join_set.join_next().await.unwrap().unwrap().unwrap();
}

#[derive(Debug, Deserialize)]
pub struct ServerConfig {
    pub access_server: Option<AccessServerSpawner>,
    pub proxy_server: Option<ProxyServerSpawner>,
}
