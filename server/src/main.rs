use access_server::{AccessServerConfig, AccessServerLoader};
use clap::Parser;
use proxy_server::{ProxyServerConfig, ProxyServerLoader};
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

    let mut access_server_loader = AccessServerLoader::new();
    let mut proxy_server_loader = ProxyServerLoader::new();
    let mut join_set = tokio::task::JoinSet::new();

    let access_server = config.access_server.unwrap_or_default();
    access_server
        .spawn_and_kill(&mut join_set, &mut access_server_loader)
        .await
        .unwrap();
    let proxy_server = config.proxy_server.unwrap_or_default();
    proxy_server
        .spawn(&mut join_set, &mut proxy_server_loader)
        .await
        .unwrap();

    join_set.join_next().await.unwrap().unwrap().unwrap();
}

#[derive(Debug, Deserialize)]
pub struct ServerConfig {
    pub access_server: Option<AccessServerConfig>,
    pub proxy_server: Option<ProxyServerConfig>,
}
