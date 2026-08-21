#![warn(clippy::disallowed_methods, clippy::disallowed_types)]

use std::{collections::HashMap, future::Future, pin::Pin, sync::Arc, time::Duration};

use access_server::{AccessServerConfig, AccessServerLoader, AccessServerLoaderSnapshot};
use ae::anti_replay::{ReplayValidator, TimeValidator};
use common::{
    anti_replay::{VALIDATOR_CAPACITY, VALIDATOR_TIME_FRAME, VALIDATOR_UDP_HDR_TTL},
    config::{Merge, merge_map},
    connect::{
        ConnectorConfig, ConnectorConfigUpdater, ConnectorResetSignal, connector_config_cell,
    },
    error::{AnyError, AnyResult},
    lifecycle::retention::RetentionActorSender,
    lifecycle::suspend::SystemSuspendSignal,
    matcher::Matcher,
    notify::Subscription,
    proto::{
        client::stream::StreamTracer,
        connect::udp::UdpConnector,
        context::{Runtime, StreamRuntime, UdpRuntime},
        metrics::{stream::StreamSessionTable, udp::UdpSessionTable},
    },
    route::{ConnConfig, ConnSelector, ProbeRtt, Registries},
    session::SessionSpawner,
    stream::pool::{StreamConnPool, StreamPoolBuilder},
};
use config::ReadConfig;
use protocol::{
    access_server::{self, PreparedAccessServer},
    proxy_server::{
        self, PreparedProxyServer, ProxyServerConfig, ProxyServerLoader, ProxyServerLoaderSnapshot,
    },
    reverse_tunnel::{
        self, PreparedReverseTunnel, ReverseTunnelConfig, ReverseTunnelLoader,
        ReverseTunnelLoaderSnapshot,
    },
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
const RELOAD_DEBOUNCE: Duration = Duration::from_secs(1);

/// A boxed, `'static` preparation future: it owns its inputs (an `Arc` config
/// reader and a snapshot of the loaders), so it never borrows live state and
/// can be stored in the machine without pinning down a lifetime.
type PrepareReloadFuture =
    Pin<Box<dyn Future<Output = Result<PreparedReload, ServerServeError>> + Send + 'static>>;

/// The server's reload machine: [`ReloadMachine`] with the real preparation
/// future.
type ServerReloadMachine = ReloadMachine<PrepareReloadFuture>;

/// The reload controller's state, carrying its data: an idle reloader, a
/// debounce window collapsing watcher bursts, or an in-flight preparation.
/// The enum makes invalid combinations unrepresentable — there is no separate
/// phase flag plus a side table of sleeps and futures — and a prepared reload
/// is handed out for a single commit rather than stored for a later phase.
enum ReloadState<Prepare> {
    /// No reload in flight; a change notification starts a debounce window.
    Idle,
    /// Collapsing rapid change notifications; expires into [`ReloadState::Preparing`].
    Debouncing(Pin<Box<tokio::time::Sleep>>),
    /// Building the next generation; the current generation keeps serving.
    Preparing(Prepare),
}

/// The in-flight reload state machine, polled from the main `select!` rather
/// than a spawned actor so server-task panics keep cascading while a reload
/// is in progress.
struct ReloadMachine<Prepare> {
    state: ReloadState<Prepare>,
}
impl<Prepare> ReloadMachine<Prepare> {
    fn new() -> Self {
        Self {
            state: ReloadState::Idle,
        }
    }
    fn is_idle(&self) -> bool {
        matches!(self.state, ReloadState::Idle)
    }
    fn is_debouncing(&self) -> bool {
        matches!(self.state, ReloadState::Debouncing(_))
    }
    #[cfg_attr(not(test), allow(dead_code))]
    fn is_preparing(&self) -> bool {
        matches!(self.state, ReloadState::Preparing(_))
    }
    /// A config-file change arrived: (re)start the debounce window,
    /// collapsing bursts of watcher events into a single reload.
    fn on_config_changed(&mut self) {
        self.state = ReloadState::Debouncing(Box::pin(tokio::time::sleep(RELOAD_DEBOUNCE)));
    }
    /// The debounce window expired; start building the next generation.
    fn begin_preparing(&mut self, prepare: Prepare) {
        self.state = ReloadState::Preparing(prepare);
    }
}
/// One resolved step of the reload controller, as driven by the serve loop.
enum ReloadStep<Prepared> {
    /// A config-file change (re)started the debounce window.
    ConfigChanged,
    /// The debounce window elapsed; begin preparing the next generation.
    DebounceElapsed,
    /// The in-flight preparation resolved; commit the reload exactly once.
    Prepared(Result<Prepared, ServerServeError>),
}

/// A future that resolves when the machine's current step completes: the
/// debounce window expires or the in-flight preparation resolves. A resolved
/// preparation returns the machine to idle before the result is handed out,
/// so the caller commits exactly once and a failure is not retried. Never
/// resolves while Idle, so it can race in the select loop without consuming
/// events meant for other states.
async fn reload_step<Prepare, Prepared>(
    machine: &mut ReloadMachine<Prepare>,
) -> ReloadStep<Prepared>
where
    Prepare: Future<Output = Result<Prepared, ServerServeError>> + Unpin,
{
    loop {
        match &mut machine.state {
            ReloadState::Debouncing(debounce) => {
                debounce.as_mut().await;
                return ReloadStep::DebounceElapsed;
            }
            ReloadState::Preparing(prepare) => {
                let result = Pin::new(prepare).await;
                // The borrow of `machine.state` (through `prepare`) ends at
                // its last use above; return to idle before handing the
                // result out for its single commit.
                machine.state = ReloadState::Idle;
                return ReloadStep::Prepared(result);
            }
            ReloadState::Idle => std::future::pending::<()>().await,
        }
    }
}

/// Race the reload machine's next step against the server-task set, exactly
/// as the serve loop does: a completed server task surfaces first — its
/// panic is resurrected, its error returned — so a failure is never parked
/// while a reload is in progress; otherwise the machine advances and the
/// resulting step is returned.
async fn drive_reload<Prepare, Prepared>(
    machine: &mut ReloadMachine<Prepare>,
    server_tasks: &mut tokio::task::JoinSet<AnyResult>,
    config_changed: &mut Subscription,
) -> Result<ReloadStep<Prepared>, ServerServeError>
where
    Prepare: Future<Output = Result<Prepared, ServerServeError>> + Unpin,
{
    loop {
        tokio::select! {
            Some(res) = server_tasks.join_next() => {
                res.unwrap().map_err(ServerServeError::ServerTask)?;
            }
            _ = config_changed.notified(), if machine.is_idle() || machine.is_debouncing() => {
                machine.on_config_changed();
                return Ok(ReloadStep::ConfigChanged);
            }
            step = reload_step(machine) => {
                return Ok(step);
            }
        }
    }
}

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
/// state. Takes its inputs by value — an `Arc` config reader and a consumed
/// [`ServerLoaderSnapshot`] — so the returned future is `'static` and can be
/// stored in the reload machine without borrowing live state.
async fn prepare_reload<CR>(
    config_reader: Arc<CR>,
    server_loader: ServerLoaderSnapshot,
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
    connector_config_updater: &ConnectorConfigUpdater,
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
    // One replacement: the updater is shared by the stream connector table,
    // the UDP connector, and every mux UDP dialer.
    connector_config_updater.replace(connector_config);
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

#[cfg(test)]
mod tests {
    use super::*;
    use common::notify::Notify;

    /// A preparation future resolved by the test via a oneshot; the prepared
    /// type is `()` so tests never need to build a real [`PreparedReload`].
    type TestPrepare = Pin<Box<dyn Future<Output = Result<(), ServerServeError>> + Send>>;

    fn test_machine() -> ReloadMachine<TestPrepare> {
        ReloadMachine::new()
    }

    /// A preparation future that resolves when the test sends a result on
    /// `rx` (a dropped sender surfaces as a config error).
    fn controlled_prepare(
        rx: tokio::sync::oneshot::Receiver<Result<(), ServerServeError>>,
    ) -> TestPrepare {
        Box::pin(async move {
            rx.await
                .map_err(|_| ServerServeError::Config("prepare sender dropped".into()))?
        })
    }

    /// A change signal and its subscription, so tests can both broadcast and
    /// observe notifications.
    fn test_signal() -> (Notify, Subscription) {
        let signal = Notify::new();
        let subscription = signal.subscription();
        (signal, subscription)
    }

    /// A config change during the debounce window resets the window instead
    /// of letting it expire.
    #[tokio::test(start_paused = true)]
    async fn a_change_during_the_debounce_window_resets_it() {
        let mut machine = test_machine();

        machine.on_config_changed();
        assert!(machine.is_debouncing());

        // Half the window passes...
        tokio::time::advance(RELOAD_DEBOUNCE / 2).await;
        // ...a second change resets the window instead of letting it expire.
        machine.on_config_changed();
        assert!(machine.is_debouncing());

        // The original deadline has passed by now, but the reset one has not.
        tokio::time::advance(RELOAD_DEBOUNCE / 2).await;
        assert!(
            machine.is_debouncing(),
            "the reset window must still be running"
        );

        // The reset window has now elapsed: the debounce sleep resolves.
        tokio::time::advance(RELOAD_DEBOUNCE / 2).await;
        let step = reload_step(&mut machine).await;
        assert!(matches!(step, ReloadStep::DebounceElapsed));
    }

    /// A config change arriving while a reload is being prepared is not
    /// lost: it stays pending and starts a fresh debounce window once the
    /// machine returns to idle.
    #[tokio::test(start_paused = true)]
    async fn a_change_during_preparation_is_not_lost() {
        let (signal, mut config_changed) = test_signal();
        let (tx, rx) = tokio::sync::oneshot::channel();
        let mut machine = test_machine();

        machine.begin_preparing(controlled_prepare(rx));
        assert!(machine.is_preparing());

        // A change arrives while preparing: the change branch is disabled
        // until the machine returns to idle, so the notification stays
        // pending.
        signal.notify_waiters();
        assert!(machine.is_preparing());

        // Preparation completes; the machine returns to idle, handing the
        // result out exactly once...
        tx.send(Ok(())).unwrap();
        let step = reload_step(&mut machine).await;
        let ReloadStep::Prepared(result) = step else {
            panic!("expected a Prepared step");
        };
        assert!(result.is_ok());
        assert!(machine.is_idle());

        // ...and the queued change starts a fresh debounce window.
        config_changed.notified().await;
        machine.on_config_changed();
        assert!(machine.is_debouncing());
    }

    /// A prepared reload is handed out exactly once: after the single
    /// hand-out the machine returns to idle, and a fresh cycle works without
    /// re-committing the old prepare.
    #[tokio::test(start_paused = true)]
    async fn a_prepared_reload_is_handed_out_exactly_once() {
        let mut machine = test_machine();
        let (tx, rx) = tokio::sync::oneshot::channel();

        machine.begin_preparing(controlled_prepare(rx));
        tx.send(Ok(())).unwrap();
        let step = reload_step(&mut machine).await;
        let ReloadStep::Prepared(result) = step else {
            panic!("expected a Prepared step");
        };
        assert!(result.is_ok());
        assert!(
            machine.is_idle(),
            "after the single hand-out the machine must be idle"
        );

        // A fresh cycle: the machine never re-commits the old prepare.
        let (tx2, rx2) = tokio::sync::oneshot::channel();
        machine.begin_preparing(controlled_prepare(rx2));
        assert!(machine.is_preparing());
        tx2.send(Err(ServerServeError::Load("boom".into())))
            .unwrap();
        let step = reload_step(&mut machine).await;
        let ReloadStep::Prepared(result) = step else {
            panic!("expected a Prepared step");
        };
        assert!(result.is_err());
        assert!(machine.is_idle());
    }

    /// A failed preparation returns to idle without retrying.
    #[tokio::test(start_paused = true)]
    async fn a_failed_preparation_returns_to_idle_without_retry() {
        let mut machine = test_machine();
        let (tx, rx) = tokio::sync::oneshot::channel();

        machine.begin_preparing(controlled_prepare(rx));
        tx.send(Err(ServerServeError::Config("synthetic".into())))
            .unwrap();
        let step = reload_step(&mut machine).await;
        let ReloadStep::Prepared(result) = step else {
            panic!("expected a Prepared step");
        };
        assert!(result.is_err());
        assert!(
            machine.is_idle(),
            "a failed preparation must return to idle, not retry"
        );
    }

    /// A server-task panic surfaces while the machine is debouncing, exactly
    /// as it does in the serve loop.
    #[tokio::test(start_paused = true)]
    #[should_panic(expected = "connector driver exploded")]
    async fn a_server_task_panic_propagates_during_debouncing() {
        let mut machine = test_machine();
        let mut server_tasks = tokio::task::JoinSet::new();
        let mut config_changed = test_signal().1;

        machine.on_config_changed();
        assert!(machine.is_debouncing());
        server_tasks.spawn(async { panic!("connector driver exploded") });
        // Let the panicking task land in the join set before driving.
        tokio::task::yield_now().await;
        let _ = drive_reload(&mut machine, &mut server_tasks, &mut config_changed).await;
    }

    /// A server-task panic surfaces while a reload is being prepared.
    #[tokio::test(start_paused = true)]
    #[should_panic(expected = "connector driver exploded")]
    async fn a_server_task_panic_propagates_during_preparation() {
        let mut machine = test_machine();
        let mut server_tasks = tokio::task::JoinSet::new();
        let mut config_changed = test_signal().1;
        let (_tx, rx) = tokio::sync::oneshot::channel();

        machine.begin_preparing(controlled_prepare(rx));
        assert!(machine.is_preparing());
        server_tasks.spawn(async { panic!("connector driver exploded") });
        tokio::task::yield_now().await;
        let _ = drive_reload(&mut machine, &mut server_tasks, &mut config_changed).await;
    }

    /// A server-task error surfaces as a [`ServerServeError::ServerTask`]
    /// while a reload is being prepared.
    #[tokio::test(start_paused = true)]
    async fn a_server_task_error_propagates_during_preparation() {
        let mut machine = test_machine();
        let mut server_tasks = tokio::task::JoinSet::new();
        let mut config_changed = test_signal().1;
        let (_tx, rx) = tokio::sync::oneshot::channel();

        machine.begin_preparing(controlled_prepare(rx));
        assert!(machine.is_preparing());
        server_tasks
            .spawn(async { Err::<(), AnyError>(std::io::Error::other("connector died").into()) });
        tokio::task::yield_now().await;
        let step = drive_reload(&mut machine, &mut server_tasks, &mut config_changed).await;
        assert!(matches!(step, Err(ServerServeError::ServerTask(_))));
    }
}
