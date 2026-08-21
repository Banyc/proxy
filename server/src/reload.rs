//! Hot-reload controller for the proxy server.
//!
//! A reload is driven by a small state machine ([`ReloadMachine`]) that
//! collapses bursts of config-file changes into a single debounced
//! preparation, builds the next generation while the current one keeps
//! serving, and commits it exactly once. [`prepare_reload`] and
//! [`commit_reload`] are the two ends of that cycle: one resolves and binds
//! the next generation without touching live state, the other swaps it in.

use std::{collections::HashMap, pin::Pin, sync::Arc, time::Duration};

use common::{
    connect::{ConnectorConfig, ConnectorConfigUpdater},
    error::{AnyError, AnyResult},
    matcher::Matcher,
    notify::Subscription,
    proxy_runtime::{client::stream::StreamTracer, context::Runtime},
    route::{Registries, RouteSelector},
    stream::pool::StreamConnPool,
};
// `ProbeRtt` is referenced only in the `dyn ProbeRtt` annotation of
// `stream_tracer` below; rustc's `unused_imports` lint mis-flags it there, yet
// removing the import is a hard error, so silence the false positive.
use crate::config::ReadConfig;
#[allow(unused_imports)]
use common::route::ProbeRtt;
use protocol::{
    access_server::{self, PreparedAccessServer},
    proxy_server::{self, PreparedProxyServer},
    reverse_tunnel::{self, PreparedReverseTunnel},
};
use tokio_util::sync::CancellationToken;

use crate::{ServerConfig, ServerLoader, ServerLoaderSnapshot, ServerServeError};

const RELOAD_DEBOUNCE: Duration = Duration::from_secs(1);

/// A boxed, `'static` preparation future: it owns its inputs (an `Arc` config
/// reader and a snapshot of the loaders), so it never borrows live state and
/// can be stored in the machine without pinning down a lifetime.
type PrepareReloadFuture =
    Pin<Box<dyn Future<Output = Result<PreparedReload, ServerServeError>> + Send + 'static>>;

/// The server's reload machine: [`ReloadMachine`] with the real preparation
/// future.
pub type ServerReloadMachine = ReloadMachine<PrepareReloadFuture>;

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
pub struct ReloadMachine<Prepare> {
    state: ReloadState<Prepare>,
}
impl<Prepare> ReloadMachine<Prepare> {
    pub fn new() -> Self {
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
    pub fn begin_preparing(&mut self, prepare: Prepare) {
        self.state = ReloadState::Preparing(prepare);
    }
}
/// One resolved step of the reload controller, as driven by the serve loop.
pub enum ReloadStep<Prepared> {
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
pub async fn drive_reload<Prepare, Prepared>(
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
pub async fn prepare_reload<CR>(
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
    let empty_conn_selector: HashMap<Arc<str>, RouteSelector> = HashMap::new();
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

pub fn commit_reload(
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ServerServeError;
    use common::notify::{Notify, Subscription};

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
