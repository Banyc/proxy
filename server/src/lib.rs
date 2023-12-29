use std::sync::Arc;

use access_server::{AccessServerConfig, AccessServerLoader};
use common::{
    error::{AnyError, AnyResult},
    session_table::BothSessionTables,
    stream::concrete::{
        addr::ConcreteStreamType,
        pool::{Pool, PoolBuilder},
    },
};
use config::ConfigReader;
use proxy_server::{ProxyServerConfig, ProxyServerLoader};
use serde::Deserialize;
use thiserror::Error;
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

pub mod config;

pub async fn serve<CR>(
    notify_rx: Arc<tokio::sync::Notify>,
    config_reader: CR,
    session_table: BothSessionTables<ConcreteStreamType>,
) -> Result<(), ServeError>
where
    CR: ConfigReader<Config = ServerConfig>,
{
    let mut server_loader = ServerLoader {
        access_server: AccessServerLoader::new(),
        proxy_server: ProxyServerLoader::new(),
    };
    let mut server_tasks = tokio::task::JoinSet::new();

    // Make sure there is always `Some(res)` from `join_next`
    server_tasks.spawn(async move {
        let (_tx, rx) = tokio::sync::oneshot::channel::<()>();
        rx.await.unwrap();
        unreachable!()
    });

    let stream_pool = Pool::empty();

    let cancellation = CancellationToken::new();
    read_and_load_config(
        &config_reader,
        &mut server_tasks,
        &mut server_loader,
        stream_pool.clone(),
        cancellation.clone(),
        session_table.clone(),
    )
    .await?;

    let mut _cancellation_guard = cancellation.drop_guard();

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

                let cancellation = CancellationToken::new();
                if let Err(e) = read_and_load_config(
                    &config_reader,
                    &mut server_tasks,
                    &mut server_loader,
                    stream_pool.clone(),
                    cancellation.clone(),
                    session_table.clone(),
                ).await {
                    warn!(?e, "Failed to read and load config");
                    continue;
                }

                _cancellation_guard = cancellation.drop_guard();
            }
        }
    }
}

pub struct ServerLoader {
    pub access_server: AccessServerLoader,
    pub proxy_server: ProxyServerLoader,
}

async fn read_and_load_config<CR>(
    config_reader: &CR,
    server_tasks: &mut tokio::task::JoinSet<AnyResult>,
    server_loader: &mut ServerLoader,
    stream_pool: Pool,
    cancellation: CancellationToken,
    session_table: BothSessionTables<ConcreteStreamType>,
) -> Result<(), ServeError>
where
    CR: ConfigReader<Config = ServerConfig>,
{
    let config = config_reader
        .read_config()
        .await
        .map_err(ServeError::Config)?;
    load_and_clean(
        config,
        server_tasks,
        server_loader,
        stream_pool,
        cancellation,
        session_table,
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

pub async fn load_and_clean(
    config: ServerConfig,
    server_tasks: &mut tokio::task::JoinSet<AnyResult>,
    server_loader: &mut ServerLoader,
    stream_pool: Pool,
    cancellation: CancellationToken,
    session_table: BothSessionTables<ConcreteStreamType>,
) -> AnyResult {
    stream_pool.replaced_by(config.global.stream_pool.build()?);

    config
        .access_server
        .spawn_and_clean(
            server_tasks,
            &mut server_loader.access_server,
            &stream_pool,
            cancellation,
            session_table,
        )
        .await?;
    config
        .proxy_server
        .load_and_clean(server_tasks, &mut server_loader.proxy_server, &stream_pool)
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
