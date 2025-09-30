use std::{
    collections::HashMap,
    sync::{Arc, RwLock},
};

use access_server::{AccessServerConfig, AccessServerLoader};
use ae::anti_replay::{ReplayValidator, TimeValidator};
use common::{
    anti_replay::{VALIDATOR_CAPACITY, VALIDATOR_TIME_FRAME, VALIDATOR_UDP_HDR_TTL},
    config::{Merge, merge_map},
    connect::{ConnectorConfig, ConnectorReset},
    error::{AnyError, AnyResult},
    proto::{
        connect::udp::UdpConnector,
        context::{Context, StreamContext, UdpContext},
        metrics::{stream::StreamSessionTable, udp::UdpSessionTable},
        route::{StreamConnConfigBuilder, UdpConnConfigBuilder},
    },
    stream::pool::{StreamConnPool, StreamPoolBuilder},
    suspend::Suspended,
};
use config::ReadConfig;
use protocol::{
    access_server,
    proxy_server::{self, ProxyServerConfig, ProxyServerLoader},
    stream::connect::build_concrete_stream_connector_table,
};
use serde::Deserialize;
use swap::Swap;
use thiserror::Error;
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

use crate::config::ConfigChanged;

pub mod config;
pub mod monitor;
pub mod profiling;

pub struct ServeContext {
    pub stream_session_table: Option<StreamSessionTable>,
    pub udp_session_table: Option<UdpSessionTable>,
    pub config_changed: ConfigChanged,
    pub system_suspended: Suspended,
}

pub async fn serve<CR>(config_reader: CR, serve_context: ServeContext) -> Result<(), ServeError>
where
    CR: ReadConfig<Config = ServerConfig>,
{
    let mut server_loader = ServerLoader {
        access_server: AccessServerLoader::new(),
        proxy_server: ProxyServerLoader::new(),
    };
    let mut server_tasks = tokio::task::JoinSet::new();

    let stream_pool = Swap::new(StreamConnPool::empty());
    let stream_validator = Arc::new(ReplayValidator::new(
        VALIDATOR_TIME_FRAME,
        VALIDATOR_CAPACITY,
    ));
    let udp_validator = Arc::new(TimeValidator::new(
        VALIDATOR_TIME_FRAME + VALIDATOR_UDP_HDR_TTL,
    ));
    let connector_reset = ConnectorReset(serve_context.system_suspended.0);
    let context = Context {
        stream: StreamContext {
            session_table: serve_context.stream_session_table,
            pool: stream_pool,
            connector_table: Arc::new(build_concrete_stream_connector_table(
                ConnectorConfig::default(),
                connector_reset,
            )),
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
    let mut config_changed = serve_context.config_changed.0.waiter();

    loop {
        tokio::select! {
            Some(res) = server_tasks.join_next() => {
                let res = res.unwrap();
                res.map_err(ServeError::ServerTask)?;
            }
            _ = config_changed.notified() => {
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
    context: Context,
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
    context: Context,
) -> AnyResult {
    let mut stream_conn = HashMap::new();
    for (k, v) in config.stream.conn {
        let v = v.build()?;
        stream_conn.insert(k, v);
    }

    let mut udp_conn = HashMap::new();
    for (k, v) in config.udp.conn {
        let v = v.build()?;
        udp_conn.insert(k, v);
    }

    context.stream.pool.replaced_by(
        config
            .stream
            .pool
            .build(context.stream.connector_table.clone(), &stream_conn)?,
    );
    context
        .stream
        .connector_table
        .replaced_by(config.connector.clone());
    *context.udp.connector.config().write().unwrap() = config.connector.clone();

    access_server::spawn_and_clean(
        config.access_server,
        server_tasks,
        &mut server_loader.access_server,
        cancellation,
        context.clone(),
        &stream_conn,
        &udp_conn,
    )
    .await?;
    proxy_server::spawn_and_clean(
        config.proxy_server,
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
    pool: StreamPoolBuilder,
    #[serde(default)]
    #[serde(alias = "proxy_server")]
    conn: HashMap<Arc<str>, StreamConnConfigBuilder>,
}
impl Merge for StreamConfig {
    type Error = AnyError;

    fn merge(self, other: Self) -> Result<Self, Self::Error>
    where
        Self: Sized,
    {
        let pool = self.pool.merge(other.pool)?;
        let conn = merge_map(self.conn, other.conn)?;
        Ok(Self { pool, conn })
    }
}

#[derive(Debug, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct UdpConfig {
    #[serde(default)]
    #[serde(alias = "proxy_server")]
    conn: HashMap<Arc<str>, UdpConnConfigBuilder>,
}
impl Merge for UdpConfig {
    type Error = AnyError;

    fn merge(self, other: Self) -> Result<Self, Self::Error>
    where
        Self: Sized,
    {
        let conn = merge_map(self.conn, other.conn)?;
        Ok(Self { conn })
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
