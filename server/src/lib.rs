use std::sync::Arc;

use access_server::{AccessServerConfig, AccessServerLoader};
use common::{
    error::{AnyError, AnyResult},
    stream::pool::PoolBuilder,
};
use config::ConfigReader;
use proxy_server::{ProxyServerConfig, ProxyServerLoader};
use serde::Deserialize;
use thiserror::Error;
use tracing::{info, warn};

pub mod config;

pub async fn serve<CR>(
    notify_rx: Arc<tokio::sync::Notify>,
    config_reader: CR,
) -> Result<(), ServeError>
where
    CR: ConfigReader<Config = ServerConfig>,
{
    let mut access_server_loader = AccessServerLoader::new();
    let mut proxy_server_loader = ProxyServerLoader::new();
    let mut server_tasks = tokio::task::JoinSet::new();

    // Make sure there is always `Some(res)` from `join_next`
    server_tasks.spawn(async move {
        let (_tx, rx) = tokio::sync::oneshot::channel::<()>();
        rx.await.unwrap();
        unreachable!()
    });

    read_and_load_config(
        &config_reader,
        &mut server_tasks,
        &mut access_server_loader,
        &mut proxy_server_loader,
    )
    .await?;

    loop {
        tokio::select! {
            res = server_tasks.join_next() => {
                let res = res.expect("Always `Some(res)` from `join_next`");
                let res = res.unwrap();
                res.map_err(ServeError::ServerTask)?;
            }
            _ = notify_rx.notified() => {
                info!("Config file changed");

                // Wait for file change to settle
                tokio::time::sleep(std::time::Duration::from_secs(1)).await;

                if let Err(e) = read_and_load_config(
                    &config_reader,
                    &mut server_tasks,
                    &mut access_server_loader,
                    &mut proxy_server_loader,
                ).await {
                    warn!(?e, "Failed to read and load config");
                }
            }
        }
    }
}

async fn read_and_load_config<CR>(
    config_reader: &CR,
    server_tasks: &mut tokio::task::JoinSet<AnyResult>,
    access_server_loader: &mut AccessServerLoader,
    proxy_server_loader: &mut ProxyServerLoader,
) -> Result<(), ServeError>
where
    CR: ConfigReader<Config = ServerConfig>,
{
    let config = config_reader
        .read_config()
        .await
        .map_err(ServeError::Config)?;
    load(
        config,
        server_tasks,
        access_server_loader,
        proxy_server_loader,
    )
    .await
    .map_err(ServeError::Load)?;
    Ok(())
}

#[derive(Debug, Error)]
pub enum ServeError {
    #[error("Failed to read config file: {0}")]
    Config(#[source] AnyError),
    #[error("Failed to load config: {0}")]
    Load(#[source] AnyError),
    #[error("Server task failed: {0}")]
    ServerTask(#[source] AnyError),
}

pub async fn load(
    config: ServerConfig,
    server_tasks: &mut tokio::task::JoinSet<AnyResult>,
    access_server_loader: &mut AccessServerLoader,
    proxy_server_loader: &mut ProxyServerLoader,
) -> AnyResult {
    let stream_pool = config.global.stream_pool.build()?;
    config
        .access_server
        .spawn_and_kill(server_tasks, access_server_loader, &stream_pool)
        .await?;
    config
        .proxy_server
        .spawn_and_kill(server_tasks, proxy_server_loader, &stream_pool)
        .await?;
    Ok(())
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServerConfig {
    #[serde(default)]
    pub access_server: AccessServerConfig,
    #[serde(default)]
    pub proxy_server: ProxyServerConfig,
    #[serde(default)]
    pub global: Global,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Global {
    pub stream_pool: PoolBuilder,
}
