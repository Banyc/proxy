use std::{
    collections::HashMap,
    sync::{Arc, RwLock},
};

use access_server::{AccessServerConfig, AccessServerLoader};
use common::{
    anti_replay::{
        ReplayValidator, TimeValidator, VALIDATOR_CAPACITY, VALIDATOR_TIME_FRAME,
        VALIDATOR_UDP_HDR_TTL,
    },
    config::{Merge, merge_map},
    connect::ConnectorConfig,
    context::Context,
    error::{AnyError, AnyResult},
    stream::metrics::StreamSessionTable,
    udp::{
        connect::UdpConnector, context::UdpContext, metrics::UdpSessionTable,
        proxy_table::UdpProxyConfigBuilder,
    },
};
use config::ReadConfig;
use protocol::{
    context::ConcreteContext,
    stream::{
        connect::ConcreteStreamConnectorTable,
        context::ConcreteStreamContext,
        pool::{ConcreteConnPool, ConcretePoolBuilder},
        proxy_table::StreamProxyConfigBuilder,
    },
};
use proxy_server::{ProxyServerConfig, ProxyServerLoader};
use serde::Deserialize;
use swap::Swap;
use thiserror::Error;
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

pub mod config;
pub mod monitor;
pub mod profiling;

pub struct ServeContext {
    pub stream_session_table: Option<StreamSessionTable>,
    pub udp_session_table: Option<UdpSessionTable>,
}

pub async fn serve<CR>(
    notify_rx: Arc<tokio::sync::Notify>,
    config_reader: CR,
    serve_context: ServeContext,
) -> Result<(), ServeError>
where
    CR: ReadConfig<Config = ServerConfig>,
{
    let mut server_loader = ServerLoader {
        access_server: AccessServerLoader::new(),
        proxy_server: ProxyServerLoader::new(),
    };
    let mut server_tasks = tokio::task::JoinSet::new();

    let stream_pool = Swap::new(ConcreteConnPool::empty());
    let stream_validator = Arc::new(ReplayValidator::new(
        VALIDATOR_TIME_FRAME,
        VALIDATOR_CAPACITY,
    ));
    let udp_validator = Arc::new(TimeValidator::new(
        VALIDATOR_TIME_FRAME + VALIDATOR_UDP_HDR_TTL,
    ));
    let context = Context {
        stream: ConcreteStreamContext {
            session_table: serve_context.stream_session_table,
            pool: stream_pool,
            connector_table: Arc::new(
                ConcreteStreamConnectorTable::new(ConnectorConfig::default()),
            ),
            replay_validator: Arc::clone(&stream_validator),
        },
        udp: UdpContext {
            session_table: serve_context.udp_session_table,
            time_validator: Arc::clone(&udp_validator),
            connector: Arc::new(UdpConnector::new(Arc::new(RwLock::new(
                ConnectorConfig::default(),
            )))),
        },
    };

    let cancellation = CancellationToken::new();
    read_and_exec_config(
        &config_reader,
        &mut server_tasks,
        &mut server_loader,
        cancellation.clone(),
        context.clone(),
    )
    .await?;

    let mut _cancellation_guard = cancellation.drop_guard();

    loop {
        tokio::select! {
            Some(res) = server_tasks.join_next() => {
                let res = res.unwrap();
                res.map_err(ServeError::ServerTask)?;
            }
            _ = notify_rx.notified() => {
                info!("Config file changed");

                // Wait for file change to settle
                tokio::time::sleep(std::time::Duration::from_secs(1)).await;

                let cancellation = CancellationToken::new();
                if let Err(e) = read_and_exec_config(
                    &config_reader,
                    &mut server_tasks,
                    &mut server_loader,
                    cancellation.clone(),
                    context.clone(),
                ).await {
                    warn!(?e, "Failed to read and execute config");
                    continue;
                }

                _cancellation_guard = cancellation.drop_guard();
            }
        }
    }
}

/// Spawn and kill servers given a new config
pub struct ServerLoader {
    pub access_server: AccessServerLoader,
    pub proxy_server: ProxyServerLoader,
}

async fn read_and_exec_config<CR>(
    config_reader: &CR,
    server_tasks: &mut tokio::task::JoinSet<AnyResult>,
    server_loader: &mut ServerLoader,
    cancellation: CancellationToken,
    context: ConcreteContext,
) -> Result<(), ServeError>
where
    CR: ReadConfig<Config = ServerConfig>,
{
    let config = config_reader
        .read_config()
        .await
        .map_err(ServeError::Config)?;
    spawn_and_clean(config, server_tasks, server_loader, cancellation, context)
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

pub async fn spawn_and_clean(
    config: ServerConfig,
    server_tasks: &mut tokio::task::JoinSet<AnyResult>,
    server_loader: &mut ServerLoader,
    cancellation: CancellationToken,
    context: ConcreteContext,
) -> AnyResult {
    let mut stream_proxy_server = HashMap::new();
    for (k, v) in config.stream.proxy_server {
        let v = v.build()?;
        stream_proxy_server.insert(k, v);
    }

    let mut udp_proxy = HashMap::new();
    for (k, v) in config.udp.proxy_server {
        let v = v.build()?;
        udp_proxy.insert(k, v);
    }

    context.stream.pool.replaced_by(
        config
            .stream
            .pool
            .build(context.stream.connector_table.clone(), &stream_proxy_server)?,
    );
    context
        .stream
        .connector_table
        .replaced_by(config.connector.clone());
    *context.udp.connector.config().write().unwrap() = config.connector.clone();

    config
        .access_server
        .spawn_and_clean(
            server_tasks,
            &mut server_loader.access_server,
            cancellation,
            context.clone(),
            &stream_proxy_server,
            &udp_proxy,
        )
        .await?;
    config
        .proxy_server
        .spawn_and_clean(
            server_tasks,
            &mut server_loader.proxy_server,
            context.clone(),
        )
        .await?;
    Ok(())
}

#[derive(Debug, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct StreamConfig {
    #[serde(default)]
    pool: ConcretePoolBuilder,
    #[serde(default)]
    proxy_server: HashMap<Arc<str>, StreamProxyConfigBuilder>,
}
impl Merge for StreamConfig {
    type Error = AnyError;

    fn merge(self, other: Self) -> Result<Self, Self::Error>
    where
        Self: Sized,
    {
        let pool = self.pool.merge(other.pool)?;
        let proxy_server = merge_map(self.proxy_server, other.proxy_server)?;
        Ok(Self { pool, proxy_server })
    }
}

#[derive(Debug, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct UdpConfig {
    #[serde(default)]
    proxy_server: HashMap<Arc<str>, UdpProxyConfigBuilder>,
}
impl Merge for UdpConfig {
    type Error = AnyError;

    fn merge(self, other: Self) -> Result<Self, Self::Error>
    where
        Self: Sized,
    {
        let proxy_server = merge_map(self.proxy_server, other.proxy_server)?;
        Ok(Self { proxy_server })
    }
}

#[derive(Debug, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct ServerConfig {
    #[serde(default)]
    pub connector: ConnectorConfig,
    #[serde(default)]
    pub access_server: AccessServerConfig,
    #[serde(default)]
    pub proxy_server: ProxyServerConfig,
    #[serde(default)]
    pub stream: StreamConfig,
    #[serde(default)]
    pub udp: UdpConfig,
}
impl Merge for ServerConfig {
    type Error = AnyError;

    fn merge(self, other: Self) -> Result<Self, Self::Error>
    where
        Self: Sized,
    {
        let connector = self.connector.merge(other.connector)?;
        let access_server = self.access_server.merge(other.access_server)?;
        let proxy_server = self.proxy_server.merge(other.proxy_server)?;
        let stream = self.stream.merge(other.stream)?;
        let udp = self.udp.merge(other.udp)?;
        Ok(Self {
            access_server,
            proxy_server,
            stream,
            udp,
            connector,
        })
    }
}
