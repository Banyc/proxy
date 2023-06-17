use std::{io, sync::Arc};

use access_server::{AccessServerConfig, AccessServerLoader};
use async_trait::async_trait;
use clap::Parser;
use common::error::{AnyError, AnyResult};
use file_watcher_tokio::EventActor;
use proxy_server::{ProxyServerConfig, ProxyServerLoader};
use serde::Deserialize;
use tracing::{error, info};

#[derive(Debug, Parser)]
struct Args {
    /// Paths to the configuration files.
    config_files: Vec<String>,
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt::init();
    let args = Args::parse();

    let config = read_config(&args.config_files).await.unwrap();

    let watcher = ConfigWatcher::new();
    let notify_rx = Arc::clone(watcher.notify_rx());
    let watcher = Arc::new(watcher);
    for file in &args.config_files {
        let file = file.clone();
        let watcher = Arc::clone(&watcher);
        tokio::spawn(async move { file_watcher_tokio::watch_file(file, watcher).await });
    }

    let mut access_server_loader = AccessServerLoader::new();
    let mut proxy_server_loader = ProxyServerLoader::new();
    let mut join_set = tokio::task::JoinSet::new();

    load(
        config,
        &mut join_set,
        &mut access_server_loader,
        &mut proxy_server_loader,
    )
    .await
    .unwrap();

    loop {
        tokio::select! {
            res = join_set.join_next() => {
                res.expect("No servers running").unwrap().unwrap();
            }
            _ = notify_rx.notified() => {
                info!("Config file changed");

                let config = match read_config(&args.config_files).await {
                    Ok(config) => config,
                    Err(e) => {
                        error!(?e, "Failed to read config file");
                        continue;
                    }
                };

                load(config, &mut join_set, &mut access_server_loader, &mut proxy_server_loader).await.unwrap();
            }
        }
    }
}

async fn read_config(config_files: &[String]) -> Result<ServerConfig, AnyError> {
    let mut config_str = String::new();
    for config_file in config_files {
        let src = tokio::fs::read_to_string(config_file).await?;
        config_str.push_str(&src);
    }
    let config: ServerConfig = toml::from_str(&config_str)?;
    Ok(config)
}

async fn load(
    config: ServerConfig,
    join_set: &mut tokio::task::JoinSet<AnyResult>,
    access_server_loader: &mut AccessServerLoader,
    proxy_server_loader: &mut ProxyServerLoader,
) -> io::Result<()> {
    let access_server = config.access_server.unwrap_or_default();
    access_server
        .spawn_and_kill(join_set, access_server_loader)
        .await?;
    let proxy_server = config.proxy_server.unwrap_or_default();
    proxy_server.spawn(join_set, proxy_server_loader).await?;
    Ok(())
}

#[derive(Debug, Deserialize)]
pub struct ServerConfig {
    pub access_server: Option<AccessServerConfig>,
    pub proxy_server: Option<ProxyServerConfig>,
}

struct ConfigWatcher {
    tx: Arc<tokio::sync::Notify>,
}

impl ConfigWatcher {
    pub fn new() -> Self {
        let tx = Arc::new(tokio::sync::Notify::new());
        Self { tx }
    }

    pub fn notify_rx(&self) -> &Arc<tokio::sync::Notify> {
        &self.tx
    }
}

#[async_trait]
impl EventActor for ConfigWatcher {
    async fn notify(&self, _event: notify::Event) {
        self.tx.notify_one();
    }
}
