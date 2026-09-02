#![warn(clippy::disallowed_methods, clippy::disallowed_types)]

use std::{collections::HashMap, sync::Arc};

use access_server::{AccessServerConfig, AccessServerLoader, AccessServerLoaderSnapshot};
use ae::anti_replay::{ReplayValidator, TimeValidator};
use common::{
    anti_replay::{VALIDATOR_CAPACITY, VALIDATOR_TIME_FRAME, VALIDATOR_UDP_HDR_TTL},
    config::{Merge, merge_map},
    connect::{ConnectorConfig, ConnectorResetSignal, connector_config_cell},
    error::AnyError,
    lifecycle::retention::RetentionActorSender,
    lifecycle::suspend::SystemResumeSignal,
    proxy_runtime::{
        connect::udp::UdpConnector,
        context::{Runtime, StreamRuntime, UdpRuntime},
        metrics::{stream::StreamSessionTable, udp::UdpSessionTable},
    },
    route::HopConfig,
    session::SessionSpawner,
    stream_runtime::pool::{StreamConnPool, StreamPoolBuilder},
};
use config::ReadConfig;
use protocol::{
    access_server::{self},
    proxy_server::{ProxyServerConfig, ProxyServerLoader, ProxyServerLoaderSnapshot},
    reverse_tunnel::{ReverseTunnelConfig, ReverseTunnelLoader, ReverseTunnelLoaderSnapshot},
    stream_proto::connect::build_concrete_stream_connector_table,
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
pub mod reload;

pub struct ServeContext {
    pub stream_session_table: Option<StreamSessionTable>,
    pub udp_session_table: Option<UdpSessionTable>,
    pub config_changed: ConfigChangeSignal,
    pub system_resume: SystemResumeSignal,
    pub retention: RetentionActorSender,
}

pub async fn serve<CR>(
    config_reader: CR,
    serve_context: ServeContext,
) -> Result<(), ServerServeError>
where
    CR: ReadConfig<Config = ServerConfig> + Send + Sync + 'static,
{
    // Reload machinery now lives in `crate::reload`; pull the items used by
    // this serve loop into scope.
    use crate::reload::{
        ReloadStep, ServerReloadMachine, commit_reload, drive_reload, prepare_reload,
    };

    let config_reader = Arc::new(config_reader);
    let (session_spawner, mut session_rx) = SessionSpawner::channel();
    let mut sessions = tokio::task::JoinSet::new();
    let mut server_loader = ServerLoader {
        access_server: AccessServerLoader::new(),
        proxy_server: ProxyServerLoader::new(),
        reverse_tunnel: ReverseTunnelLoader::new(),
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
    let connector_reset = ConnectorResetSignal(serve_context.system_resume.0);
    // One connector-configuration cell shared by the stream connector table,
    // the UDP connector, and every mux UDP dialer: a reload replaces it in a
    // single write, so stream and UDP connectors can never observe different
    // configurations. The reload path holds the sole updater; every
    // connector holds a reader clone.
    let (connector_config_reader, connector_config_updater) =
        connector_config_cell(ConnectorConfig::default());
    let udp_connector = Arc::new(UdpConnector::new(connector_config_reader.clone()));
    let stream_connector_table = Arc::new(build_concrete_stream_connector_table(
        connector_config_reader.clone(),
        connector_reset,
        &mut server_tasks,
        &udp_connector,
    ));
    let runtime = Runtime {
        session_spawner: session_spawner.clone(),
        stream: StreamRuntime {
            session_table: serve_context.stream_session_table,
            pool: stream_pool,
            connector_table: stream_connector_table,
            replay_validator: Arc::clone(&stream_validator),
            session_spawner: session_spawner.clone(),
            retention: serve_context.retention.clone(),
        },
        udp: UdpRuntime {
            session_table: serve_context.udp_session_table,
            time_validator: Arc::clone(&udp_validator),
            connector: udp_connector,
            session_spawner: session_spawner.clone(),
            retention: serve_context.retention.clone(),
        },
    };

    let cancellation = CancellationToken::new();
    // Initial configuration preparation: race the first preparation against
    // the connector drivers already spawned into `server_tasks`, so a
    // connector-driver panic or failure during startup surfaces immediately
    // instead of parking until the serve loop begins.
    let prepared = {
        let prepare = prepare_reload(
            Arc::clone(&config_reader),
            server_loader.snapshot(),
            cancellation.clone(),
            runtime.clone(),
        );
        tokio::pin!(prepare);
        loop {
            tokio::select! {
                res = &mut prepare => break res?,
                Some(res) = server_tasks.join_next() => {
                    // Surface connector-driver failures and panics during
                    // startup instead of parking them.
                    res.unwrap().map_err(ServerServeError::ServerTask)?;
                }
            }
        }
    };
    let (guard, commit_error) = commit_reload(
        &mut server_tasks,
        &mut server_loader,
        prepared,
        &runtime,
        &connector_config_updater,
    );
    if let Some(e) = commit_error {
        return Err(ServerServeError::Commit(e));
    }
    let mut _cancellation_guard = guard;
    let mut config_changed = serve_context.config_changed.0.subscription();
    let mut reload = ServerReloadMachine::new();

    let outcome = loop {
        tokio::select! {
            Some(fut) = session_rx.recv() => {
                sessions.spawn(fut);
            }
            Some(res) = sessions.join_next() => {
                if let Err(error) = res.unwrap() {
                    error!(?error, "Session task returned an error");
                }
            }
            step = drive_reload(&mut reload, &mut server_tasks, &mut config_changed) => {
                match step {
                    Ok(ReloadStep::ConfigChanged) => {
                        info!("Config file changed");
                    }
                    Ok(ReloadStep::DebounceElapsed) => {
                        // The debounce window expired; start building the
                        // next generation while the current one keeps
                        // serving. The prepare future owns its inputs (an
                        // `Arc` config reader and a snapshot of the
                        // loaders), so it is `'static` and never borrows
                        // live state.
                        reload.begin_preparing(Box::pin(prepare_reload(
                            Arc::clone(&config_reader),
                            server_loader.snapshot(),
                            CancellationToken::new(),
                            runtime.clone(),
                        )));
                    }
                    Ok(ReloadStep::Prepared(result)) => match result {
                        Ok(prepared) => {
                            // Commit the prepared reload exactly once, then
                            // return to idle; a failed commit is reported
                            // and not retried.
                            let (guard, commit_error) = commit_reload(
                                &mut server_tasks,
                                &mut server_loader,
                                prepared,
                                &runtime,
                                &connector_config_updater,
                            );
                            _cancellation_guard = guard;
                            if let Some(e) = commit_error {
                                error!(
                                    ?e,
                                    "Reload commit partially failed: a listener died; \
                                     its handler update was lost; new generation installed"
                                );
                            }
                        }
                        Err(e) => {
                            error!(?e, "Failed to prepare reload; live config unchanged");
                        }
                    },
                    Err(e) => break Err(e),
                }
            }
        }
    };
    // Fatal-outcome epilog: stop admitting sessions, adopt every future that
    // is still queued, then abort and reap the session and server task sets
    // with logging so a completed panic is not hidden by a JoinSet drop.
    session_rx.close();
    while let Some(fut) = session_rx.recv().await {
        sessions.spawn(fut);
    }
    common::lifecycle::task_scope::abort_and_reap_with(&mut sessions, |res| {
        if let Err(error) = res {
            error!(?error, "Session task returned an error during shutdown");
        }
    })
    .await;
    common::lifecycle::task_scope::abort_and_reap_with(&mut server_tasks, |res| {
        if let Err(error) = res {
            error!(?error, "Server task returned an error during shutdown");
        }
    })
    .await;
    outcome
}

/// The window that collapses bursts of watcher events into one reload.
pub struct ServerLoader {
    pub access_server: AccessServerLoader,
    pub proxy_server: ProxyServerLoader,
    pub reverse_tunnel: ReverseTunnelLoader,
}
impl ServerLoader {
    /// A read-only snapshot of the live loaders, for preparation. The
    /// snapshot resolves against the same live listeners but cannot commit.
    pub fn snapshot(&self) -> ServerLoaderSnapshot {
        ServerLoaderSnapshot {
            access_server: self.access_server.snapshot(),
            proxy_server: self.proxy_server.snapshot(),
            reverse_tunnel: self.reverse_tunnel.snapshot(),
        }
    }
}

/// An immutable snapshot of the live [`ServerLoader`]s, taken by
/// [`ServerLoader::snapshot`] for preparation. Preparation can resolve and
/// bind builders against the live listener set, but it cannot commit —
/// replacement authority stays with the single owning [`ServerLoader`].
pub struct ServerLoaderSnapshot {
    pub access_server: AccessServerLoaderSnapshot,
    pub proxy_server: ProxyServerLoaderSnapshot,
    pub reverse_tunnel: ReverseTunnelLoaderSnapshot,
}

#[derive(Debug, Error)]
pub enum ServerServeError {
    #[error("Failed to read config file: {0}")]
    Config(#[source] AnyError),
    #[error("Failed to load config: {0}")]
    Load(#[source] AnyError),
    #[error("Failed to commit reload: {0}")]
    Commit(#[source] AnyError),
    #[error("Server task failed: {0}")]
    ServerTask(#[source] AnyError),
}

#[derive(Debug, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct StreamConfig {
    #[serde(default)]
    pool: StreamPoolBuilder,
    #[serde(default)]
    #[serde(alias = "conn", alias = "proxy_server")]
    upstream: HashMap<Arc<str>, HopConfig>,
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
    upstream: HashMap<Arc<str>, HopConfig>,
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
    pub reverse_tunnel: ReverseTunnelConfig,
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
        let reverse_tunnel = self.reverse_tunnel.merge(other.reverse_tunnel)?;
        let stream = self.stream.merge(other.stream)?;
        let udp = self.udp.merge(other.udp)?;
        Ok(Self {
            access_server,
            proxy_server,
            reverse_tunnel,
            stream,
            udp,
            connector,
        })
    }
}
