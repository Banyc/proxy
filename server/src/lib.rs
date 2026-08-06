#![warn(clippy::disallowed_methods, clippy::disallowed_types)]

use std::{
    collections::HashMap,
    sync::{Arc, RwLock},
};

use access_server::{AccessServerConfig, AccessServerLoader};
use ae::anti_replay::{ReplayValidator, TimeValidator};
use common::{
    anti_replay::{VALIDATOR_CAPACITY, VALIDATOR_TIME_FRAME, VALIDATOR_UDP_HDR_TTL},
    config::{Merge, merge_map},
    connect::{ConnectorConfig, ConnectorResetSignal},
    error::{AnyError, AnyResult},
    matcher::Matcher,
    proto::{
        client::stream::StreamTracer,
        connect::udp::UdpConnector,
        context::{Runtime, StreamRuntime, UdpRuntime},
        metrics::{stream::StreamSessionTable, udp::UdpSessionTable},
    },
    retention::RetentionActorSender,
    route::{ConnConfig, ConnSelector, ProbeRtt, Registries},
    session::SessionSpawner,
    stream::pool::{StreamConnPool, StreamPoolBuilder},
    suspend::SystemSuspendSignal,
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
use tracing::{error, info};

use crate::config::ConfigChangeSignal;

pub mod config;
pub mod monitor;
pub mod profiling;

pub struct ServeContext {
    pub stream_session_table: Option<StreamSessionTable>,
    pub udp_session_table: Option<UdpSessionTable>,
    pub config_changed: ConfigChangeSignal,
    pub system_suspended: SystemSuspendSignal,
    pub retention: RetentionActorSender,
}

pub async fn serve<CR>(
    config_reader: CR,
    serve_context: ServeContext,
) -> Result<(), ServerServeError>
where
    CR: ReadConfig<Config = ServerConfig>,
{
    let (session_spawner, mut session_rx) = SessionSpawner::channel();
    let mut sessions = tokio::task::JoinSet::new();
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
    let connector_reset = ConnectorResetSignal(serve_context.system_suspended.0);
    let runtime = Runtime {
        session_spawner: session_spawner.clone(),
        stream: StreamRuntime {
            session_table: serve_context.stream_session_table,
            pool: stream_pool,
            connector_table: Arc::new(build_concrete_stream_connector_table(
                ConnectorConfig::default(),
                connector_reset,
            )),
            replay_validator: Arc::clone(&stream_validator),
            session_spawner: session_spawner.clone(),
            retention: serve_context.retention.clone(),
        },
        udp: UdpRuntime {
            session_table: serve_context.udp_session_table,
            time_validator: Arc::clone(&udp_validator),
            connector: Arc::new(UdpConnector::new(Arc::new(RwLock::new(
                ConnectorConfig::default(),
            )))),
            session_spawner: session_spawner.clone(),
            retention: serve_context.retention.clone(),
        },
    };

    let cancellation = CancellationToken::new();
    read_and_exec_config(
        &config_reader,
        &mut server_tasks,
        &mut server_loader,
        cancellation.clone(),
        runtime.clone(),
    )
    .await?;

    let mut _cancellation_guard = cancellation.drop_guard();
    let mut config_changed = serve_context.config_changed.0.subscription();

    loop {
        tokio::select! {
            Some(res) = server_tasks.join_next() => {
                let res = match res {
                    Ok(res) => res,
                    Err(error) if error.is_panic() => {
                        error!(?error, "Server task panicked");
                        std::panic::resume_unwind(error.into_panic());
                    }
                    Err(error) => {
                        error!(?error, "Server task failed to join");
                        continue;
                    }
                };
                res.map_err(ServerServeError::ServerTask)?;
            }
            Some(fut) = session_rx.recv() => {
                sessions.spawn(fut);
            }
            Some(res) = sessions.join_next() => {
                let res = match res {
                    Ok(res) => res,
                    Err(error) if error.is_panic() => {
                        error!(?error, "Session task panicked");
                        std::panic::resume_unwind(error.into_panic());
                    }
                    Err(error) => {
                        error!(?error, "Session task failed to join");
                        continue;
                    }
                };
                if let Err(e) = res {
                    error!(?e, "Session task returned an error");
                }
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
                    runtime.clone(),
                ).await {
                    error!(?e, "Failed to read and execute config");
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
    runtime: Runtime,
) -> Result<(), ServerServeError>
where
    CR: ReadConfig<Config = ServerConfig>,
{
    let config = config_reader
        .read_config()
        .await
        .map_err(ServerServeError::Config)?;
    spawn_and_clean(config, server_tasks, server_loader, cancellation, runtime)
        .await
        .map_err(ServerServeError::Load)?;
    Ok(())
}

#[derive(Debug, Error)]
pub enum ServerServeError {
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
    runtime: Runtime,
) -> AnyResult {
    let stream_conn = config.stream.upstream;
    let udp_conn = config.udp.upstream;

    let stream_tracer: Arc<dyn ProbeRtt + Send + Sync> =
        Arc::new(StreamTracer::new(runtime.stream.clone()));
    let empty_matcher: Arc<HashMap<Arc<str>, Matcher>> = Arc::new(HashMap::new());
    let empty_conn_selector: HashMap<Arc<str>, ConnSelector> = HashMap::new();
    let stream_registries = Registries {
        conn: &stream_conn,
        matcher: &empty_matcher,
        conn_selector: &empty_conn_selector,
        tracer: &stream_tracer,
        connector_table: &runtime.stream.connector_table,
        cancellation: cancellation.clone(),
    };
    runtime
        .stream
        .pool
        .replaced_by(config.stream.pool.resolve(&stream_registries)?);
    runtime
        .stream
        .connector_table
        .replaced_by(config.connector.clone());
    *runtime.udp.connector.config().write().unwrap() = config.connector.clone();

    access_server::spawn_and_clean(
        config.access_server,
        server_tasks,
        &mut server_loader.access_server,
        cancellation,
        runtime.clone(),
        &stream_conn,
        &udp_conn,
    )
    .await?;
    proxy_server::spawn_and_clean(
        config.proxy_server,
        server_tasks,
        &mut server_loader.proxy_server,
        runtime.clone(),
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
    #[serde(alias = "conn", alias = "proxy_server")]
    upstream: HashMap<Arc<str>, ConnConfig>,
}
impl Merge for StreamConfig {
    type Error = AnyError;

    fn merge(self, other: Self) -> Result<Self, Self::Error>
    where
        Self: Sized,
    {
        let pool = self.pool.merge(other.pool)?;
        let upstream = merge_map(self.upstream, other.upstream)?;
        Ok(Self { pool, upstream })
    }
}

#[derive(Debug, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct UdpConfig {
    #[serde(default)]
    #[serde(alias = "conn", alias = "proxy_server")]
    upstream: HashMap<Arc<str>, ConnConfig>,
}
impl Merge for UdpConfig {
    type Error = AnyError;

    fn merge(self, other: Self) -> Result<Self, Self::Error>
    where
        Self: Sized,
    {
        let upstream = merge_map(self.upstream, other.upstream)?;
        Ok(Self { upstream })
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
