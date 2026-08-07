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
    access_server::{self, PreparedAccessServer},
    proxy_server::{self, PreparedProxyServer, ProxyServerConfig, ProxyServerLoader},
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
                &mut server_tasks,
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
    let prepared = prepare_reload(
        &config_reader,
        &server_loader,
        cancellation.clone(),
        runtime.clone(),
    )
    .await?;
    // No live state exists yet — commit immediately.
    let mut _cancellation_guard =
        commit_reload(&mut server_tasks, &mut server_loader, prepared, &runtime)
            .map_err(ServerServeError::Commit)?;
    let mut config_changed = serve_context.config_changed.0.subscription();

    loop {
        tokio::select! {
            Some(res) = server_tasks.join_next() => {
                res.unwrap().map_err(ServerServeError::ServerTask)?;
            }
            Some(fut) = session_rx.recv() => {
                sessions.spawn(fut);
            }
            Some(res) = sessions.join_next() => {
                res.unwrap();
            }
            _ = config_changed.notified() => {
                info!("Config file changed");

                // Wait for file change to settle
                tokio::time::sleep(std::time::Duration::from_secs(1)).await;

                let cancellation = CancellationToken::new();
                match prepare_reload(
                    &config_reader,
                    &server_loader,
                    cancellation.clone(),
                    runtime.clone(),
                ).await {
                    Ok(new_prepared) => {
                        // Commit: atomically swap global state, hot-swap handlers,
                        // spawn new listener tasks, and cancel the previous
                        // generation's token. Reassigning `_cancellation_guard`
                        // drops the old guard, cancelling the old token.
                        match commit_reload(
                            &mut server_tasks,
                            &mut server_loader,
                            new_prepared,
                            &runtime,
                        ) {
                            Ok(guard) => _cancellation_guard = guard,
                            Err(e) => {
                                // A listener died between prepare and commit, so
                                // its handler update could not be delivered.
                                // The global state was already swapped; install
                                // the new generation's guard (canceling the old
                                // probe tasks) and surface the lost update.
                                error!(
                                    ?e,
                                    "Reload commit partially failed: a listener \
                                     died; its handler update was lost"
                                );
                                continue;
                            }
                        }
                    }
                    Err(e) => {
                        // `prepare_reload` already dropped the failed
                        // `PreparedReload` (cancelling its candidate token and
                        // aborting its probe tasks via `JoinSet` drop), so the
                        // live configuration is untouched. Keep serving with the
                        // existing generation.
                        error!(?e, "Failed to prepare reload; live config unchanged");
                        continue;
                    }
                }
            }
        }
    }
}

/// Spawn and kill servers given a new config
pub struct ServerLoader {
    pub access_server: AccessServerLoader,
    pub proxy_server: ProxyServerLoader,
}

/// A fully-prepared, not-yet-committed reload.
///
/// Everything that can fail — config parsing, route/selector resolution
/// (which spawns probe tasks tied to `cancellation`), and listener socket
/// binding — is resolved and held here without touching live state. Commit
/// via [`commit_reload`] atomically swaps global state and hot-swaps
/// listener handlers.
///
/// Dropping a `PreparedReload` without committing cancels its `cancellation`
/// token (aborting the probe tasks spawned during resolution) and drops the
/// prepared `PreparedAccessServer`/`PreparedProxyServer` (which aborts their
/// inner probe-task `JoinSet`s and drops bound sockets). No live state is
/// mutated, no listener task is spawned, and no candidate token survives a
/// failed or abandoned prepare.
pub struct PreparedReload {
    cancellation: CancellationToken,
    stream_pool: StreamConnPool,
    connector_config: ConnectorConfig,
    access_server: PreparedAccessServer,
    proxy_server: PreparedProxyServer,
}

impl PreparedReload {
    /// Consume `self`, disarming drop-cancellation, and return the parts.
    /// Used by [`commit_reload`] to move every field out without running
    /// `Drop` (so the new generation's token stays live).
    fn into_parts(self) -> PreparedReloadParts {
        // Wrap in ManuallyDrop so this struct's `Drop` (which would cancel
        // the token) does not run when the fields are moved out.
        let me = std::mem::ManuallyDrop::new(self);
        PreparedReloadParts {
            cancellation: unsafe { std::ptr::read(&me.cancellation) },
            stream_pool: unsafe { std::ptr::read(&me.stream_pool) },
            connector_config: unsafe { std::ptr::read(&me.connector_config) },
            access_server: unsafe { std::ptr::read(&me.access_server) },
            proxy_server: unsafe { std::ptr::read(&me.proxy_server) },
        }
    }
}

impl Drop for PreparedReload {
    fn drop(&mut self) {
        // Cancel the candidate token so any probe tasks spawned during
        // prepare observe cancellation promptly. The probe-task `JoinSet`s
        // owned by each `GaugedConnChain` (inside the `ConnSelector`s held by
        // the prepared access/proxy servers) are also dropped here, forcefully
        // aborting the tasks. Together this guarantees no candidate task
        // survives a failed or abandoned prepare.
        self.cancellation.cancel();
    }
}

struct PreparedReloadParts {
    cancellation: CancellationToken,
    stream_pool: StreamConnPool,
    connector_config: ConnectorConfig,
    access_server: PreparedAccessServer,
    proxy_server: PreparedProxyServer,
}

/// Phase 1 — prepare: read config, resolve routes/selectors (spawning probe
/// tasks tied to `cancellation`), bind every new listener, and build every
/// handler, all without touching live state. On any error the in-progress
/// `PreparedReload` is dropped, which cancels the candidate token and aborts
/// the probe tasks.
async fn prepare_reload<CR>(
    config_reader: &CR,
    server_loader: &ServerLoader,
    cancellation: CancellationToken,
    runtime: Runtime,
) -> Result<PreparedReload, ServerServeError>
where
    CR: ReadConfig<Config = ServerConfig>,
{
    // `guard` cancels `cancellation` on drop — including every error path
    // below — so no candidate probe task survives a failed prepare. On
    // success the guard is disarmed (`disarm`) before returning, leaving the
    // token live for the new generation.
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

    guard.disarm();
    Ok(PreparedReload {
        cancellation,
        stream_pool,
        connector_config,
        access_server,
        proxy_server,
    })
}

/// Cancels a `CancellationToken` on drop unless [`Self::disarm`]ed. Used to
/// guarantee the candidate token is cancelled on every error path of
/// [`prepare_reload`].
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

/// Phase 2 — commit: atomically swap the global pool/connector config,
/// hot-swap listener handlers, spawn new listener tasks, and drop removed
/// listener handles. Consumes `prepared` (so its `Drop` does *not* run — the
/// token stays live for the new generation). Returns a `DropGuard` for the
/// new generation's `cancellation` token; dropping the previous guard
/// cancels the previous generation's token.
///
/// Returns an error if a listener died between prepare and commit, so a
/// handler update is never silently lost. The global state is already
/// swapped at that point, but the lost update is surfaced rather than
/// swallowed.
fn commit_reload(
    server_tasks: &mut tokio::task::JoinSet<AnyResult>,
    server_loader: &mut ServerLoader,
    prepared: PreparedReload,
    runtime: &Runtime,
) -> Result<tokio_util::sync::DropGuard, AnyError> {
    let PreparedReloadParts {
        cancellation,
        stream_pool,
        connector_config,
        access_server,
        proxy_server,
    } = prepared.into_parts();
    // Swap global state first.
    runtime.stream.pool.replaced_by(stream_pool);
    runtime
        .stream
        .connector_table
        .replaced_by(connector_config.clone());
    *runtime.udp.connector.config().write().unwrap() = connector_config;
    // Commit listener handlers / tasks. A closed receiver (listener died
    // since prepare) surfaces an error instead of silently losing the
    // handler update. On failure the new generation's token is cancelled so
    // its probe tasks do not leak.
    let commit_result = (|| {
        server_loader
            .access_server
            .commit(server_tasks, access_server)?;
        server_loader
            .proxy_server
            .commit(server_tasks, proxy_server)?;
        Ok::<(), AnyError>(())
    })();
    if let Err(e) = commit_result {
        cancellation.cancel();
        return Err(e);
    }
    // Return a guard so the next reload (or `serve` returning) cancels this
    // generation's probe tasks.
    Ok(cancellation.drop_guard())
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
