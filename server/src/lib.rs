use std::{io, sync::Arc};

use access_server::{AccessServerConfig, AccessServerLoader};
use common::error::AnyResult;
use config::ConfigReader;
use proxy_server::{ProxyServerConfig, ProxyServerLoader};
use serde::Deserialize;
use tracing::{info, warn};

pub mod config;

pub async fn serve<CR>(notify_rx: Arc<tokio::sync::Notify>, config_reader: CR) -> !
where
    CR: ConfigReader<Config = ServerConfig>,
{
    let mut access_server_loader = AccessServerLoader::new();
    let mut proxy_server_loader = ProxyServerLoader::new();
    let mut join_set = tokio::task::JoinSet::new();

    let config = config_reader.read_config().await.unwrap();

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

                let config = match config_reader.read_config().await {
                    Ok(config) => config,
                    Err(e) => {
                        warn!(?e, "Failed to read config file");
                        continue;
                    }
                };

                load(config, &mut join_set, &mut access_server_loader, &mut proxy_server_loader).await.unwrap();
            }
        }
    }
}

pub async fn load(
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
    proxy_server
        .spawn_and_kill(join_set, proxy_server_loader)
        .await?;
    Ok(())
}

#[derive(Debug, Deserialize)]
pub struct ServerConfig {
    pub access_server: Option<AccessServerConfig>,
    pub proxy_server: Option<ProxyServerConfig>,
}
