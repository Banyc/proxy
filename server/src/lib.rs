#![warn(clippy::disallowed_methods, clippy::disallowed_types)]

use std::{collections::HashMap, future::Future, pin::Pin, sync::Arc, time::Duration};

use access_server::{AccessServerConfig, AccessServerLoader};
use ae::anti_replay::{ReplayValidator, TimeValidator};
use common::{
    anti_replay::{VALIDATOR_CAPACITY, VALIDATOR_TIME_FRAME, VALIDATOR_UDP_HDR_TTL},
    config::{Merge, merge_map},
    connect::{ConnectorConfig, ConnectorConfigHandle, ConnectorResetSignal},
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
    access_server::{self, PreparedAccessServer},
    proxy_server::{self, PreparedProxyServer, ProxyServerConfig, ProxyServerLoader},
    reverse_tunnel::{self, PreparedReverseTunnel, ReverseTunnelConfig, ReverseTunnelLoader},
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
    CR: ReadConfig<Config = ServerConfig> + Send + Sync + 'static,
{
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
    let connector_reset = ConnectorResetSignal(serve_context.system_suspended.0);
    // One connector-configuration cell shared by the stream connector table,
    // the UDP connector, and every mux UDP dialer: a reload replaces it in a
    // single write, so stream and UDP connectors can never observe different
    // configurations.
    let connector_config = ConnectorConfigHandle::new(ConnectorConfig::default());
    let udp_connector = Arc::new(UdpConnector::new(connector_config.clone()));
    let stream_connector_table = Arc::new(build_concrete_stream_connector_table(
        connector_config.clone(),
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
    let prepared = prepare_reload(
        Arc::clone(&config_reader),
        server_loader.clone(),
        cancellation.clone(),
        runtime.clone(),
    )
    .await?;
    let (guard, commit_error) = commit_reload(
        &mut server_tasks,
        &mut server_loader,
        prepared,
        &runtime,
        &connector_config,
    );
    if let Some(e) = commit_error {
        return Err(ServerServeError::Commit(e));
    }
    let mut _cancellation_guard = guard;
    let mut config_changed = serve_context.config_changed.0.subscription();
    let mut reload = ReloadMachine::new();

    loop {
        tokio::select! {
            Some(res) = server_tasks.join_next() => {
                res.unwrap().map_err(ServerServeError::ServerTask)?;
            }
            Some(fut) = session_rx.recv() => {
                sessions.spawn(fut);
            }
            Some(res) = sessions.join_next() => {
                if let Err(error) = res.unwrap() {
                    error!(?error, "Session task returned an error");
                }
            }
            // Idle or Debouncing: a change notification (re)starts the
            // debounce window, collapsing bursts of watcher events into a
            // single reload.
            _ = config_changed.notified(),
                if matches!(reload.phase, ReloadPhase::Idle | ReloadPhase::Debouncing) =>
            {
                info!("Config file changed");
                reload.phase = ReloadPhase::Debouncing;
                reload.debounce = Some(Box::pin(tokio::time::sleep(RELOAD_DEBOUNCE)));
            }
            // Debouncing: the window expired; start building the next
            // generation while the current one keeps serving. The prepare
            // future owns its inputs (an `Arc` config reader and a snapshot
            // of the loaders), so it is `'static` and never borrows live
            // state.
            _ = async {
                if let Some(debounce) = reload.debounce.as_mut() {
                    Pin::new(debounce).await;
                } else {
                    std::future::pending::<()>().await;
                }
            }, if reload.phase == ReloadPhase::Debouncing => {
                reload.debounce = None;
                reload.phase = ReloadPhase::Preparing;
                reload.prepare = Some(Box::pin(prepare_reload(
                    Arc::clone(&config_reader),
                    server_loader.clone(),
                    CancellationToken::new(),
                    runtime.clone(),
                )));
            }
            // Preparing: the next generation is ready. A failed prepare is
            // reported and dropped — no retry — and the live generation is
            // left untouched.
            result = async {
                match reload.prepare.as_mut() {
                    Some(prepare) => Pin::new(prepare).await,
                    None => std::future::pending().await,
                }
            }, if reload.phase == ReloadPhase::Preparing => {
                reload.prepare = None;
                match result {
                    Ok(prepared) => {
                        reload.phase = ReloadPhase::CommitOnce;
                        reload.commit_once = Some(prepared);
                    }
                    Err(e) => {
                        error!(?e, "Failed to prepare reload; live config unchanged");
                        reload.phase = ReloadPhase::Idle;
                    }
                }
            }
            // CommitOnce: install the prepared generation exactly once, then
            // return to Idle.
            _ = std::future::ready(()), if reload.phase == ReloadPhase::CommitOnce => {
                let prepared = reload
                    .commit_once
                    .take()
                    .expect("CommitOnce holds the prepared reload");
                let (guard, commit_error) = commit_reload(
                    &mut server_tasks,
                    &mut server_loader,
                    prepared,
                    &runtime,
                    &connector_config,
                );
                _cancellation_guard = guard;
                if let Some(e) = commit_error {
                    error!(
                        ?e,
                        "Reload commit partially failed: a listener died; \
                         its handler update was lost; new generation installed"
                    );
                }
                reload.phase = ReloadPhase::Idle;
            }
        }
    }
}

/// A config-file change is debounced, the next generation is prepared while
/// the current one keeps serving, and the prepared reload is committed
/// exactly once before returning to idle.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ReloadPhase {
    /// No reload in flight; a change notification starts a debounce window.
    Idle,
    /// Collapsing rapid change notifications; expires into [`ReloadPhase::Preparing`].
    Debouncing,
    /// Building the next generation; the current generation keeps serving.
    Preparing,
    /// A prepared reload awaits its single commit; commit returns to [`ReloadPhase::Idle`].
    CommitOnce,
}

/// The window that collapses bursts of watcher events into one reload.
const RELOAD_DEBOUNCE: Duration = Duration::from_secs(1);

/// A boxed, `'static` preparation future: it owns its inputs (an `Arc` config
/// reader and a snapshot of the loaders), so it never borrows live state and
/// can be stored in the machine without pinning down a lifetime.
type PrepareReloadFuture =
    Pin<Box<dyn Future<Output = Result<PreparedReload, ServerServeError>> + Send + 'static>>;

/// The in-flight reload state machine, polled from the main `select!` rather
/// than a spawned actor so server-task panics keep cascading while a reload
/// is in progress. The debounce sleep and the prepare future live in their
/// own fields so the `select!` branches can poll them without borrowing the
/// whole machine.
struct ReloadMachine {
    phase: ReloadPhase,
    debounce: Option<Pin<Box<tokio::time::Sleep>>>,
    prepare: Option<PrepareReloadFuture>,
    commit_once: Option<PreparedReload>,
}
impl ReloadMachine {
    fn new() -> Self {
        Self {
            phase: ReloadPhase::Idle,
            debounce: None,
            prepare: None,
            commit_once: None,
        }
    }
}

pub struct ServerLoader {
    pub access_server: AccessServerLoader,
    pub proxy_server: ProxyServerLoader,
    pub reverse_tunnel: ReverseTunnelLoader,
}
impl Clone for ServerLoader {
    fn clone(&self) -> Self {
        Self {
            access_server: self.access_server.clone(),
            proxy_server: self.proxy_server.clone(),
            reverse_tunnel: self.reverse_tunnel.clone(),
        }
    }
}

pub struct PreparedReload {
    parts: Option<PreparedReloadParts>,
}

impl PreparedReload {
    /// Consume the prepared reload, handing its parts to the commit path.
    /// The `Option` is taken so the [`Drop`] impl below sees an empty cell
    /// and does not cancel a token that has been committed.
    fn into_parts(mut self) -> PreparedReloadParts {
        self.parts
            .take()
            .expect("PreparedReload parts already taken")
    }
}

impl Drop for PreparedReload {
    fn drop(&mut self) {
        // A prepared reload that was never committed (an abandoned prepare or
        // a failed commit) cancels its listener-generation token; a taken one
        // is the commit path's responsibility.
        if let Some(parts) = &self.parts {
            parts.cancellation.cancel();
        }
    }
}

struct PreparedReloadParts {
    cancellation: CancellationToken,
    stream_pool: StreamConnPool,
    connector_config: ConnectorConfig,
    access_server: PreparedAccessServer,
    proxy_server: PreparedProxyServer,
    reverse_tunnel: PreparedReverseTunnel,
}

/// Read the config and build the next generation without touching live
/// state. Takes its inputs by value — an `Arc` config reader and a snapshot
/// clone of the loaders — so the returned future is `'static` and can be
/// stored in the reload machine without borrowing live state.
async fn prepare_reload<CR>(
    config_reader: Arc<CR>,
    server_loader: ServerLoader,
    cancellation: CancellationToken,
    runtime: Runtime,
) -> Result<PreparedReload, ServerServeError>
where
    CR: ReadConfig<Config = ServerConfig> + Send + Sync + 'static,
{
    let mut guard = CancelOnDrop(Some(cancellation.clone()));
    let config = config_reader
        .read_config()
        .await
        .map_err(ServerServeError::Config)?;

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
    let stream_pool = config
        .stream
        .pool
        .resolve(&stream_registries)
        .map_err(|e| ServerServeError::Load(e.into()))?;
    let connector_config = config.connector.clone();

    let access_server = access_server::prepare(
        config.access_server,
        &server_loader.access_server,
        cancellation.clone(),
        runtime.clone(),
        &stream_conn,
        &udp_conn,
    )
    .await
    .map_err(ServerServeError::Load)?;
    let proxy_server = proxy_server::prepare(
        config.proxy_server,
        &server_loader.proxy_server,
        runtime.clone(),
    )
    .await
    .map_err(ServerServeError::Load)?;
    let reverse_tunnel = reverse_tunnel::prepare(
        config.reverse_tunnel,
        &server_loader.reverse_tunnel,
        runtime.clone(),
    )
    .await
    .map_err(ServerServeError::Load)?;

    guard.disarm();
    Ok(PreparedReload {
        parts: Some(PreparedReloadParts {
            cancellation,
            stream_pool,
            connector_config,
            access_server,
            proxy_server,
            reverse_tunnel,
        }),
    })
}

struct CancelOnDrop(Option<CancellationToken>);
impl CancelOnDrop {
    fn disarm(&mut self) {
        self.0 = None;
    }
}
impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        if let Some(token) = self.0.take() {
            token.cancel();
        }
    }
}

fn commit_reload(
    server_tasks: &mut tokio::task::JoinSet<AnyResult>,
    server_loader: &mut ServerLoader,
    prepared: PreparedReload,
    runtime: &Runtime,
    connector_config_handle: &ConnectorConfigHandle,
) -> (tokio_util::sync::DropGuard, Option<AnyError>) {
    let PreparedReloadParts {
        cancellation,
        stream_pool,
        connector_config,
        access_server,
        proxy_server,
        reverse_tunnel,
    } = prepared.into_parts();
    runtime.stream.pool.replaced_by(stream_pool);
    // One replacement: the handle is shared by the stream connector table,
    // the UDP connector, and every mux UDP dialer.
    connector_config_handle.replace(connector_config);
    let commit_error: Option<AnyError> = (|| {
        server_loader
            .access_server
            .commit(server_tasks, access_server)?;
        server_loader
            .proxy_server
            .commit(server_tasks, proxy_server)?;
        server_loader
            .reverse_tunnel
            .commit(server_tasks, reverse_tunnel)?;
        Ok::<(), AnyError>(())
    })()
    .err();
    (cancellation.drop_guard(), commit_error)
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
