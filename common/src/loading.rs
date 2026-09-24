use std::{
    collections::{HashMap, HashSet},
    future::Future,
    pin::Pin,
    sync::{Arc, RwLock},
};

use derive_more::Debug;
use tokio::sync::watch;

use crate::error::{AnyError, AnyResult};

type SpawnFuture = Pin<Box<dyn Future<Output = AnyResult> + Send>>;

/// A listener loader that spawns and kills listeners
#[derive(Debug)]
pub struct Loader<ConnHandler> {
    /// Handles of the listeners using the actor model pattern
    handles: HashMap<Arc<str>, ReplaceConnHandlerTx<ConnHandler>>,
}

/// An immutable snapshot of a [`Loader`]'s live listener handles, taken by
/// [`Loader::snapshot`] for preparation. It can resolve and bind builders
/// against the live listener set, but it cannot commit — replacement
/// authority stays with the single owning [`Loader`], and the snapshot is
/// read-only: it has no way to spawn, replace, or drop a listener.
#[derive(Debug)]
pub struct LoaderSnapshot<ConnHandler> {
    handles: HashMap<Arc<str>, ReplaceConnHandlerTx<ConnHandler>>,
}

impl<ConnHandler> Loader<ConnHandler>
where
    ConnHandler: HandleConn + std::fmt::Debug + Send + Sync + 'static,
{
    pub fn new() -> Self {
        Self {
            handles: HashMap::new(),
        }
    }

    pub async fn spawn_and_clean<Server, Builder>(
        &mut self,
        join_set: &mut tokio::task::JoinSet<AnyResult>,
        builders: Vec<Builder>,
    ) -> AnyResult
    where
        Server: Serve<ConnHandler = ConnHandler> + Send + 'static,
        Builder: Build<ConnHandler = ConnHandler, Server = Server>,
    {
        let prepared = self.snapshot().prepare(builders).await?;
        self.commit(join_set, prepared)
    }

    /// A read-only snapshot of the live listener handles, for preparation.
    /// The snapshot shares the [`ReplaceConnHandlerTx`]s, so it resolves
    /// against the same live listeners, but it cannot commit.
    pub fn snapshot(&self) -> LoaderSnapshot<ConnHandler> {
        LoaderSnapshot {
            handles: self.handles.clone(),
        }
    }

    /// Apply a prepared reload: send new handlers to existing listeners,
    /// spawn new listener tasks, install new handles, and drop handles for
    /// listeners that are no longer in the config.
    ///
    /// Every op is attempted even when an earlier one fails. The ops of one
    /// preparation are independent — each carries the bound server or the
    /// replacement channel it was prepared with, and touches only its own key —
    /// so a listener that died since preparation must not forfeit the handler
    /// updates of the listeners that follow it in the preparation order. The
    /// failures are collected, each listener named, so the returned error means
    /// "these listeners lost a handler update" rather than "the reload stopped
    /// here". The caller is responsible for surfacing the failure (the global
    /// state may already have been swapped, but the lost update is reported
    /// rather than swallowed).
    ///
    /// The retirement below still runs, so a listener the new configuration
    /// removed stops serving even when a sibling op failed. A key the new
    /// configuration still names keeps its handle even when its update could
    /// not be delivered; that handle is closed, so the next preparation sees a
    /// dead listener and re-spawns it.
    pub fn commit(
        &mut self,
        join_set: &mut tokio::task::JoinSet<AnyResult>,
        prepared: PreparedOps<ConnHandler>,
    ) -> Result<(), AnyError> {
        let PreparedOps { ops, keys } = prepared;
        let mut failures: Vec<(Arc<str>, AnyError)> = Vec::new();
        for op in ops {
            match op {
                PreparedOp::Replace {
                    key,
                    tx,
                    conn_handler,
                } => {
                    // A closed receiver means the listener died since prepare.
                    // The handler update cannot be delivered — fail loudly
                    // rather than silently dropping it, and keep going: the
                    // listeners after this one can still take theirs.
                    if tx.send(conn_handler).is_err() {
                        failures.push((key, AnyError::from("listener died before reload commit")));
                    }
                }
                PreparedOp::Spawn { key, tx, spawn } => {
                    self.handles.insert(key, tx);
                    join_set.spawn(spawn);
                }
            }
        }
        // Retirement is not part of any failed step: a listener the new
        // configuration removed must stop serving whether or not a sibling
        // op failed in this commit.
        self.handles.retain(|cur_key, _| keys.contains(cur_key));
        commit_failure(failures)
    }
}

/// The error a commit reports when one or more listeners lost their handler
/// update. The message names every listener that did and preserves each
/// listener's own cause, so both the count and the cause are on the line an
/// operator reads.
fn commit_failure(failures: Vec<(Arc<str>, AnyError)>) -> Result<(), AnyError> {
    if failures.is_empty() {
        return Ok(());
    }
    let detail = failures
        .iter()
        .map(|(key, error)| format!("{key}: {error}"))
        .collect::<Vec<_>>()
        .join("; ");
    Err(format!(
        "reload commit lost handler updates for {} listener(s): {detail}",
        failures.len()
    )
    .into())
}

/// The error a preparation reports when the configuration names one listener
/// twice. The key is the listener's identity — for every config-driven
/// listener it is the `listen_addr` string — so two entries for one key cannot
/// both serve, and the message names the key that has to be dropped.
fn duplicate_key_error(key: &str) -> AnyError {
    format!("duplicate listener configuration key `{key}`").into()
}

impl<ConnHandler> LoaderSnapshot<ConnHandler>
where
    ConnHandler: HandleConn + std::fmt::Debug + Send + Sync + 'static,
{
    /// Resolve and bind every listener in `builders` against the snapshot's
    /// live handles without mutating live state.
    ///
    /// For an existing live listener a [`PreparedOp::Replace`] is produced
    /// carrying the freshly-built handler to send over the existing channel,
    /// together with the listener's key so a commit that cannot deliver the
    /// handler can name the listener it lost.
    /// For a new listener a [`PreparedOp::Spawn`] is produced carrying the
    /// bound `Server` and a fresh `ReplaceConnHandlerTx`; the server task is
    /// *not* spawned yet.
    ///
    /// A configuration that names one key twice is rejected before anything
    /// is bound: the key is the listener's identity, so two entries for one
    /// key cannot both serve — the later entry would only shadow the earlier
    /// one, leaving a socket this preparation bound and nobody accepts on.
    ///
    /// Dropping the returned [`PreparedOps`] without a commit simply drops
    /// the bound servers and handlers — no live state is touched and no task
    /// is spawned.
    pub async fn prepare<Server, Builder>(
        &self,
        builders: Vec<Builder>,
    ) -> Result<PreparedOps<ConnHandler>, AnyError>
    where
        Server: Serve<ConnHandler = ConnHandler> + Send + 'static,
        Builder: Build<ConnHandler = ConnHandler, Server = Server>,
    {
        let mut keys = HashSet::with_capacity(builders.len());
        for builder in &builders {
            if !keys.insert(builder.key().to_owned()) {
                return Err(duplicate_key_error(builder.key()));
            }
        }
        let mut ops = Vec::with_capacity(builders.len());
        for builder in builders {
            let key = builder.key().to_owned();
            let live = self.handles.get(&key);
            if live.is_some_and(|h| h.is_closed()) {
                // dead listener — treat as new so it is re-spawned below
            } else if let Some(handle) = live {
                let conn_handler = builder.build_conn_handler()?;
                ops.push(PreparedOp::Replace {
                    key: key.clone(),
                    tx: handle.clone(),
                    conn_handler,
                });
                continue;
            }
            let (set_conn_handler_tx, set_conn_handler_rx) = replace_conn_handler_channel();
            let server = builder.build_server().await.map_err(|e| {
                // Name the listener that failed so a port collision is
                // diagnosable: every config-driven listener binds through
                // this prepare.
                crate::error::bind_error(key.as_ref(), &*AnyError::from(e))
            })?;
            ops.push(PreparedOp::Spawn {
                key: key.clone(),
                tx: set_conn_handler_tx,
                spawn: Box::pin(async move {
                    server.serve(set_conn_handler_rx).await?;
                    Ok(())
                }),
            });
        }
        Ok(PreparedOps { ops, keys })
    }
}
impl<ConnHandler> Default for Loader<ConnHandler>
where
    ConnHandler: HandleConn + std::fmt::Debug + Send + Sync + 'static,
{
    fn default() -> Self {
        Self::new()
    }
}

/// A fully-prepared reload for a single [`Loader`]: bound servers and
/// built handlers ready to commit, plus the set of keys that should
/// survive the reload.
pub struct PreparedOps<ConnHandler> {
    ops: Vec<PreparedOp<ConnHandler>>,
    keys: HashSet<Arc<str>>,
}

pub enum PreparedOp<ConnHandler> {
    /// Hot-swap the handler of an existing live listener. `key` is the
    /// listener's loader key, so a commit that cannot deliver the handler can
    /// name the listener whose update it lost.
    Replace {
        key: Arc<str>,
        tx: ReplaceConnHandlerTx<ConnHandler>,
        conn_handler: ConnHandler,
    },
    /// Spawn a new listener task. `spawn` is a boxed
    /// `server.serve(rx)` future.
    Spawn {
        key: Arc<str>,
        tx: ReplaceConnHandlerTx<ConnHandler>,
        spawn: SpawnFuture,
    },
}

/// The business logic for the accepted connections
pub trait HandleConn {}

/// A builder of a server and its hook
pub trait Build {
    type ConnHandler: HandleConn;
    type Server: Serve<ConnHandler = Self::ConnHandler>;
    type Err: std::error::Error + Send + Sync + 'static;
    fn build_server(self) -> impl Future<Output = Result<Self::Server, Self::Err>> + Send;
    fn build_conn_handler(self) -> Result<Self::ConnHandler, Self::Err>;
    fn key(&self) -> &Arc<str>;
}

/// A listener including the business logic for the accepted connections
pub trait Serve {
    type ConnHandler: HandleConn;
    /// If the other end of `set_conn_handler_rx` is dropped, the listener must despawn eventually but still keep all its connections alive.
    fn serve(
        self,
        set_conn_handler_rx: ReplaceConnHandlerRx<Self::ConnHandler>,
    ) -> impl Future<Output = AnyResult> + Send;
}

/// The shared, reloadable handler cell behind a listener, replacing the
/// hand-rolled `Arc<RwLock<Arc<H>>>` in every server.
///
/// Reloads replace the handler through [`Self::replace`], and every reader —
/// a UDP packet dispatcher, a mux accepter, a flow handler — observes the
/// same current handler through [`Self::current`]. A [`watch`] generation
/// counter bumps on every replacement, so a reloader can wait until a
/// specific reload has actually been applied instead of guessing with a
/// sleep.
#[derive(Debug)]
#[debug(bound(H:))]
pub struct ReloadableHandler<H> {
    handler: Arc<RwLock<Arc<H>>>,
    /// Bumped on every [`Self::replace`]; [`Self::generation`] subscribers
    /// resolve once a replacement has been applied.
    generation: watch::Sender<u64>,
}

impl<H> Clone for ReloadableHandler<H> {
    fn clone(&self) -> Self {
        Self {
            handler: Arc::clone(&self.handler),
            generation: self.generation.clone(),
        }
    }
}

impl<H> ReloadableHandler<H> {
    /// A cell holding `handler`.
    pub fn new(handler: H) -> Self {
        let (generation, _) = watch::channel(0);
        Self {
            handler: Arc::new(RwLock::new(Arc::new(handler))),
            generation,
        }
    }

    /// The current handler.
    pub fn current(&self) -> Arc<H> {
        Arc::clone(&self.handler.read().unwrap())
    }

    /// Replace the current handler and bump the generation. `handler` is the
    /// shared `Arc` the reload channels deliver.
    pub fn replace(&self, handler: Arc<H>) {
        *self.handler.write().unwrap() = handler;
        // Copy the generation out of the watch's read lock before
        // `send_replace` takes its write lock; holding the read borrow across
        // the write would deadlock (std RwLock is not reentrant).
        let next = self.generation.borrow().wrapping_add(1);
        self.generation.send_replace(next);
    }

    /// Subscribe to reload generations: `changed()` / `wait_for()` resolve
    /// after the next replacement, giving deterministic acknowledgement that
    /// a reload was applied.
    pub fn generation(&self) -> watch::Receiver<u64> {
        self.generation.subscribe()
    }
}

#[derive(Debug)]
#[debug(bound(ConnHandler:))]
pub struct ReplaceConnHandlerTx<ConnHandler>(watch::Sender<Option<Arc<ConnHandler>>>);
impl<ConnHandler> Clone for ReplaceConnHandlerTx<ConnHandler> {
    fn clone(&self) -> Self {
        Self(self.0.clone())
    }
}
impl<ConnHandler> ReplaceConnHandlerTx<ConnHandler> {
    /// Deliver a new handler to the listener. Returns `Err(conn_handler)` if
    /// the listener has already dropped its receiver (i.e. the listener
    /// died), so a reload commit can never *silently* lose a handler update.
    pub fn send(&self, conn_handler: ConnHandler) -> Result<(), ConnHandler> {
        let arc = Arc::new(conn_handler);
        match self.0.send(Some(arc)) {
            Ok(()) => Ok(()),
            Err(watch::error::SendError(value)) => {
                // `send` fails only when `is_closed()` (no receivers). The
                // value is the `Some(Arc<ConnHandler>)` we just built; the
                // Arc has a single strong owner (we just created it and
                // `send` does not clone it on the failure path), so unwrap
                // it back to the owned `ConnHandler`.
                let arc = value.expect("we always send Some");
                Err(Arc::try_unwrap(arc).unwrap_or_else(|arc| {
                    let inner = Arc::into_inner(arc);
                    inner.expect("the Arc has a single strong owner on the send-failure path")
                }))
            }
        }
    }
    /// `true` if the listener has dropped its receiver (the listener died).
    pub fn is_closed(&self) -> bool {
        self.0.is_closed()
    }
}
#[derive(Debug)]
#[debug(bound(ConnHandler:))]
pub struct ReplaceConnHandlerRx<ConnHandler>(watch::Receiver<Option<Arc<ConnHandler>>>);
impl<ConnHandler> ReplaceConnHandlerRx<ConnHandler> {
    /// Wait for the next handler replacement. Returns:
    /// - `Ok(Some(handler))` — a new handler was delivered.
    /// - `Ok(None)` — a sentinel (no replacement); ignored by callers.
    /// - `Err(())` — all senders were dropped; the listener should despawn.
    #[allow(clippy::result_unit_err)] // `()` is a zero-information "despawn" sentinel
    pub async fn recv(&mut self) -> Result<Option<Arc<ConnHandler>>, ()> {
        match self.0.changed().await {
            Ok(()) => Ok(self.0.borrow().as_ref().cloned()),
            Err(_) => Err(()),
        }
    }
}
pub fn replace_conn_handler_channel<ConnHandler>() -> (
    ReplaceConnHandlerTx<ConnHandler>,
    ReplaceConnHandlerRx<ConnHandler>,
) {
    let (tx, rx) = watch::channel(None);
    (ReplaceConnHandlerTx(tx), ReplaceConnHandlerRx(rx))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    struct NoopConnHandler;
    impl std::fmt::Debug for NoopConnHandler {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            write!(f, "NoopConnHandler")
        }
    }
    impl HandleConn for NoopConnHandler {}
    struct DiesImmediately;
    impl Serve for DiesImmediately {
        type ConnHandler = NoopConnHandler;
        async fn serve(self, _rx: ReplaceConnHandlerRx<Self::ConnHandler>) -> AnyResult {
            Ok(())
        }
    }
    struct DiesImmediatelyBuilder {
        key: Arc<str>,
        spawns: Arc<AtomicUsize>,
    }
    impl Build for DiesImmediatelyBuilder {
        type ConnHandler = NoopConnHandler;
        type Server = DiesImmediately;
        type Err = std::io::Error;
        async fn build_server(self) -> Result<Self::Server, Self::Err> {
            self.spawns.fetch_add(1, Ordering::SeqCst);
            Ok(DiesImmediately)
        }
        fn build_conn_handler(self) -> Result<Self::ConnHandler, Self::Err> {
            Ok(NoopConnHandler)
        }
        fn key(&self) -> &Arc<str> {
            &self.key
        }
    }
    #[tokio::test]
    async fn a_reload_respawns_a_listener_that_already_died() {
        let spawns = Arc::new(AtomicUsize::new(0));
        let mut loader = Loader::new();
        let mut join_set = tokio::task::JoinSet::new();
        let builder = || DiesImmediatelyBuilder {
            key: "listener".into(),
            spawns: Arc::clone(&spawns),
        };
        loader
            .spawn_and_clean(&mut join_set, vec![builder()])
            .await
            .unwrap();
        join_set.join_next().await.unwrap().unwrap().unwrap();
        loader
            .spawn_and_clean(&mut join_set, vec![builder()])
            .await
            .unwrap();
        assert_eq!(spawns.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn a_failed_prepare_does_not_mutate_live_handles() {
        // A first reload installs one live listener at "listener".
        let spawns = Arc::new(AtomicUsize::new(0));
        let mut loader = Loader::new();
        let mut join_set = tokio::task::JoinSet::new();
        loader
            .spawn_and_clean::<DiesImmediately, DiesImmediatelyBuilder>(
                &mut join_set,
                vec![DiesImmediatelyBuilder {
                    key: "listener".into(),
                    spawns: Arc::clone(&spawns),
                }],
            )
            .await
            .unwrap();
        assert_eq!(spawns.load(Ordering::SeqCst), 1);
        // A second prepare with a *failing* builder for a *new* key must not
        // touch live state: dropping the failed PreparedOps must not spawn or
        // remove anything.
        struct FailingBuilder {
            key: Arc<str>,
        }
        impl Build for FailingBuilder {
            type ConnHandler = NoopConnHandler;
            type Server = DiesImmediately;
            type Err = std::io::Error;
            async fn build_server(self) -> Result<Self::Server, Self::Err> {
                Err(std::io::Error::other("synthetic bind failure"))
            }
            fn build_conn_handler(self) -> Result<Self::ConnHandler, Self::Err> {
                Ok(NoopConnHandler)
            }
            fn key(&self) -> &Arc<str> {
                &self.key
            }
        }
        let prepared = loader
            .snapshot()
            .prepare::<DiesImmediately, FailingBuilder>(vec![FailingBuilder {
                key: "new_listener".into(),
            }])
            .await;
        assert!(prepared.is_err(), "prepare should fail on bind error");
        // Live state is untouched: the original listener is still installed
        // and no extra spawn happened.
        assert_eq!(spawns.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn commit_fails_when_a_listener_dies_between_prepare_and_commit() {
        use std::sync::Mutex;
        // Install a listener that holds its receiver until signalled to drop
        // it, so prepare sees it as live and builds a Replace op.
        struct Lingering {
            drop_rx: Arc<Mutex<Option<tokio::sync::oneshot::Receiver<()>>>>,
        }
        impl Serve for Lingering {
            type ConnHandler = NoopConnHandler;
            async fn serve(self, mut rx: ReplaceConnHandlerRx<Self::ConnHandler>) -> AnyResult {
                let mut drop_rx = self.drop_rx.lock().unwrap().take().unwrap();
                tokio::select! {
                    biased;
                    _ = rx.recv() => {}
                    _ = &mut drop_rx => {}
                }
                Ok(())
            }
        }
        struct LingeringBuilder {
            key: Arc<str>,
            drop_rx: Arc<Mutex<Option<tokio::sync::oneshot::Receiver<()>>>>,
        }
        impl Build for LingeringBuilder {
            type ConnHandler = NoopConnHandler;
            type Server = Lingering;
            type Err = std::io::Error;
            async fn build_server(self) -> Result<Self::Server, Self::Err> {
                Ok(Lingering {
                    drop_rx: Arc::clone(&self.drop_rx),
                })
            }
            fn build_conn_handler(self) -> Result<Self::ConnHandler, Self::Err> {
                Ok(NoopConnHandler)
            }
            fn key(&self) -> &Arc<str> {
                &self.key
            }
        }
        let (drop_tx, drop_rx) = tokio::sync::oneshot::channel::<()>();
        let drop_rx = Arc::new(Mutex::new(Some(drop_rx)));
        let mut loader = Loader::new();
        let mut join_set = tokio::task::JoinSet::new();
        // Install the lingering listener.
        loader
            .commit(
                &mut join_set,
                loader
                    .snapshot()
                    .prepare::<Lingering, LingeringBuilder>(vec![LingeringBuilder {
                        key: "lingering".into(),
                        drop_rx: Arc::clone(&drop_rx),
                    }])
                    .await
                    .unwrap(),
            )
            .unwrap();
        // Prepare a replacement handler for the same key (listener still
        // alive, so prepare builds a Replace op).
        let prepared = loader
            .snapshot()
            .prepare::<Lingering, LingeringBuilder>(vec![LingeringBuilder {
                key: "lingering".into(),
                drop_rx: Arc::clone(&drop_rx),
            }])
            .await
            .unwrap();
        // Now kill the listener: it drops its receiver.
        drop(drop_tx);
        // Let the listener task run to completion.
        join_set.join_next().await.unwrap().unwrap().unwrap();
        // Commit must fail: the receiver is closed and the handler update
        // would be silently lost.
        let err = loader
            .commit(&mut join_set, prepared)
            .expect_err("commit must fail when the listener died");
        assert!(
            err.to_string().contains("listener died"),
            "unexpected error: {err}"
        );
    }

    #[tokio::test]
    async fn a_failed_commit_still_retires_a_listener_the_config_removed() {
        // A listener that runs until signalled, or until its handle is
        // dropped (its receiver closes). The signal lets the test kill this
        // one listener between prepare and commit without touching its peers.
        struct UntilSignalled {
            stop: tokio::sync::oneshot::Receiver<()>,
        }
        impl Serve for UntilSignalled {
            type ConnHandler = NoopConnHandler;
            async fn serve(mut self, mut rx: ReplaceConnHandlerRx<Self::ConnHandler>) -> AnyResult {
                loop {
                    tokio::select! {
                        _ = &mut self.stop => break,
                        result = rx.recv() => if result.is_err() {
                            break;
                        },
                    }
                }
                Ok(())
            }
        }
        struct UntilSignalledBuilder {
            key: Arc<str>,
            stop: tokio::sync::oneshot::Receiver<()>,
        }
        impl Build for UntilSignalledBuilder {
            type ConnHandler = NoopConnHandler;
            type Server = UntilSignalled;
            type Err = std::io::Error;
            async fn build_server(self) -> Result<Self::Server, Self::Err> {
                Ok(UntilSignalled { stop: self.stop })
            }
            fn build_conn_handler(self) -> Result<Self::ConnHandler, Self::Err> {
                Ok(NoopConnHandler)
            }
            fn key(&self) -> &Arc<str> {
                &self.key
            }
        }
        // Two live listeners: `watched` is still in the next configuration,
        // `removed` is dropped from it.
        let (watched_stop_tx, watched_stop_rx) = tokio::sync::oneshot::channel::<()>();
        let (_removed_stop_tx, removed_stop_rx) = tokio::sync::oneshot::channel::<()>();
        let mut loader = Loader::new();
        let mut join_set = tokio::task::JoinSet::new();
        loader
            .commit(
                &mut join_set,
                loader
                    .snapshot()
                    .prepare::<UntilSignalled, UntilSignalledBuilder>(vec![
                        UntilSignalledBuilder {
                            key: "watched".into(),
                            stop: watched_stop_rx,
                        },
                        UntilSignalledBuilder {
                            key: "removed".into(),
                            stop: removed_stop_rx,
                        },
                    ])
                    .await
                    .unwrap(),
            )
            .unwrap();
        assert_eq!(loader.handles.len(), 2);

        // A configuration that keeps only `watched`, so this commit must
        // retire `removed`. `watched` is still alive here, which is what
        // makes prepare build a Replace op for it.
        let (_unused_stop_tx, unused_stop_rx) = tokio::sync::oneshot::channel::<()>();
        let prepared = loader
            .snapshot()
            .prepare::<UntilSignalled, UntilSignalledBuilder>(vec![UntilSignalledBuilder {
                key: "watched".into(),
                stop: unused_stop_rx,
            }])
            .await
            .unwrap();
        // Kill `watched` between prepare and commit, so its handler
        // replacement fails and the op loop stops before retirement.
        drop(watched_stop_tx);
        join_set.join_next().await.unwrap().unwrap().unwrap();

        let err = loader
            .commit(&mut join_set, prepared)
            .expect_err("commit must fail when the listener died");
        assert!(
            err.to_string().contains("listener died"),
            "unexpected error: {err}"
        );

        // The listener the new configuration removed is retired even though
        // the commit failed: its handle is dropped, so its receiver closes
        // and its task completes.
        assert!(
            !loader.handles.contains_key("removed"),
            "the removed listener's handle must not survive a failed commit"
        );
        tokio::time::timeout(std::time::Duration::from_secs(5), join_set.join_next())
            .await
            .expect("the removed listener must despawn once its handle is dropped")
            .unwrap()
            .unwrap()
            .unwrap();

        // ...and the listener whose update failed keeps its handle, so the
        // next preparation sees a dead listener and re-spawns it.
        assert!(
            loader.handles.contains_key("watched"),
            "the failed key must keep its handle so the next prepare re-spawns it"
        );
    }

    /// A conn handler carrying the token of the generation that built it, so
    /// which generation a listener is serving is read off the token rather
    /// than off the listener still being alive.
    struct TokenConnHandler(u8);
    impl std::fmt::Debug for TokenConnHandler {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            write!(f, "TokenConnHandler({})", self.0)
        }
    }
    impl HandleConn for TokenConnHandler {}

    /// A listener that records the token of every handler it is given, and
    /// runs until it is signalled to stop or its handle is dropped (its
    /// receiver closes) — the two ways a listener goes away in production.
    struct Recording {
        stop: tokio::sync::oneshot::Receiver<()>,
        delivered: tokio::sync::mpsc::Sender<u8>,
    }
    impl Serve for Recording {
        type ConnHandler = TokenConnHandler;
        async fn serve(mut self, mut rx: ReplaceConnHandlerRx<Self::ConnHandler>) -> AnyResult {
            loop {
                tokio::select! {
                    _ = &mut self.stop => break,
                    received = rx.recv() => match received {
                        Err(()) => break,
                        Ok(Some(handler)) => {
                            self.delivered.send(handler.0).await.ok();
                        }
                        Ok(None) => {}
                    },
                }
            }
            Ok(())
        }
    }
    struct RecordingBuilder {
        key: Arc<str>,
        token: u8,
        stop: tokio::sync::oneshot::Receiver<()>,
        delivered: tokio::sync::mpsc::Sender<u8>,
    }
    impl Build for RecordingBuilder {
        type ConnHandler = TokenConnHandler;
        type Server = Recording;
        type Err = std::io::Error;
        async fn build_server(self) -> Result<Self::Server, Self::Err> {
            Ok(Recording {
                stop: self.stop,
                delivered: self.delivered,
            })
        }
        fn build_conn_handler(self) -> Result<Self::ConnHandler, Self::Err> {
            Ok(TokenConnHandler(self.token))
        }
        fn key(&self) -> &Arc<str> {
            &self.key
        }
    }

    #[tokio::test]
    async fn a_commit_applies_the_ops_that_follow_the_listeners_that_died() {
        // Three listeners of one loader, each recording the token of the
        // handler it is serving on its own channel, so which generation a
        // listener adopted is content rather than liveness.
        let (alpha_delivered, mut alpha_tokens) = tokio::sync::mpsc::channel::<u8>(8);
        let (beta_delivered, mut beta_tokens) = tokio::sync::mpsc::channel::<u8>(8);
        let (gamma_delivered, mut gamma_tokens) = tokio::sync::mpsc::channel::<u8>(8);
        let (alpha_stop, alpha_stop_rx) = tokio::sync::oneshot::channel::<()>();
        let (_alpha_stop2, alpha_stop2_rx) = tokio::sync::oneshot::channel::<()>();
        let (beta_stop, beta_stop_rx) = tokio::sync::oneshot::channel::<()>();
        let (_beta_stop2, beta_stop2_rx) = tokio::sync::oneshot::channel::<()>();
        let (_gamma_stop, gamma_stop_rx) = tokio::sync::oneshot::channel::<()>();
        let (_gamma_stop2, gamma_stop2_rx) = tokio::sync::oneshot::channel::<()>();
        let mut loader = Loader::new();
        let mut join_set = tokio::task::JoinSet::new();

        // Generation 1: three listeners, all built on token 1. A spawn carries
        // its handler inside the bound server, so nothing is delivered yet.
        let prepared = loader
            .snapshot()
            .prepare::<Recording, RecordingBuilder>(vec![
                RecordingBuilder {
                    key: "alpha".into(),
                    token: 1,
                    stop: alpha_stop_rx,
                    delivered: alpha_delivered.clone(),
                },
                RecordingBuilder {
                    key: "beta".into(),
                    token: 1,
                    stop: beta_stop_rx,
                    delivered: beta_delivered.clone(),
                },
                RecordingBuilder {
                    key: "gamma".into(),
                    token: 1,
                    stop: gamma_stop_rx,
                    delivered: gamma_delivered.clone(),
                },
            ])
            .await
            .unwrap();
        loader.commit(&mut join_set, prepared).unwrap();
        assert_eq!(loader.handles.len(), 3);

        // Generation 2: the same three listeners re-pointed at token 2. All
        // three are alive, so preparation builds a replacement for each, in
        // this order. The replacement path consumes only the handler, so the
        // delivered senders it drops end here too — once the originals are
        // dropped as well, nothing can reach a channel again.
        let prepared = loader
            .snapshot()
            .prepare::<Recording, RecordingBuilder>(vec![
                RecordingBuilder {
                    key: "alpha".into(),
                    token: 2,
                    stop: alpha_stop2_rx,
                    delivered: alpha_delivered.clone(),
                },
                RecordingBuilder {
                    key: "beta".into(),
                    token: 2,
                    stop: beta_stop2_rx,
                    delivered: beta_delivered.clone(),
                },
                RecordingBuilder {
                    key: "gamma".into(),
                    token: 2,
                    stop: gamma_stop2_rx,
                    delivered: gamma_delivered.clone(),
                },
            ])
            .await
            .unwrap();
        drop(alpha_delivered);
        drop(beta_delivered);
        drop(gamma_delivered);

        // Kill the first two between preparation and commit — they drop their
        // handler receivers, the state a listener that died on its own leaves
        // behind — and reap them, leaving `gamma` serving.
        drop(alpha_stop);
        drop(beta_stop);
        join_set.join_next().await.unwrap().unwrap().unwrap();
        join_set.join_next().await.unwrap().unwrap().unwrap();

        let err = loader
            .commit(&mut join_set, prepared)
            .expect_err("a handler update that cannot be delivered must be reported");
        // Both failures are on the error, in preparation order, and the
        // listener that did take its handler is absent from it.
        assert!(
            err.to_string().contains(
                "lost handler updates for 2 listener(s): alpha: listener died before reload \
                 commit; beta: listener died before reload commit"
            ),
            "the reported failure must name every listener whose update was lost; got: {err}"
        );
        assert!(
            !err.to_string().contains("gamma"),
            "a listener that took its handler must not be reported as having lost it; got: {err}"
        );

        // The op that followed the failed ones is applied: `gamma` serves the
        // handler generation 2 built for it, and its token is what says so — a
        // stale handler would deliver token 1, and each listener has a channel
        // of its own, so none can answer for another.
        assert_eq!(
            tokio::time::timeout(std::time::Duration::from_secs(5), gamma_tokens.recv())
                .await
                .expect("gamma must be delivered the new handler"),
            Some(2),
            "the listener after the ones that died must adopt the new handler"
        );

        // Nothing reached the listeners that died: every sender for their
        // channels is gone, so no stale handler can masquerade as a fresh one
        // there.
        for (name, tokens) in [("alpha", &mut alpha_tokens), ("beta", &mut beta_tokens)] {
            assert_eq!(
                tokens.try_recv(),
                Err(tokio::sync::mpsc::error::TryRecvError::Disconnected),
                "a handler was delivered to {name}, which had already died"
            );
        }

        // Every key of the new configuration keeps its handle: the two closed
        // ones so the next preparation re-spawns them, the live one because it
        // is serving.
        for key in ["alpha", "beta", "gamma"] {
            assert!(
                loader.handles.contains_key(key),
                "{key} must keep its handle so a failed commit can be followed by a re-spawn"
            );
        }

        join_set.shutdown().await;
    }

    // -- listeners that bind a real socket and serve a token ----------------

    /// Binds the builder's key as a real listener and answers every accepted
    /// connection with `token`, so which address serves which token is content
    /// rather than liveness. It ends when its handler channel closes — the way
    /// a listener a preparation dropped goes away.
    struct BindingServer {
        listener: tokio::net::TcpListener,
        token: u8,
    }
    impl Serve for BindingServer {
        type ConnHandler = TokenConnHandler;
        async fn serve(self, mut rx: ReplaceConnHandlerRx<Self::ConnHandler>) -> AnyResult {
            use tokio::io::AsyncWriteExt;
            loop {
                tokio::select! {
                    accepted = self.listener.accept() => {
                        let (mut stream, _peer) = accepted?;
                        let _ = stream.write_all(&[self.token]).await;
                    }
                    received = rx.recv() => {
                        if received.is_err() {
                            break;
                        }
                    }
                }
            }
            Ok(())
        }
    }

    /// Records every address it binds, so a preparation that binds a listener
    /// it then drops is read as an address rather than inferred.
    struct BindingBuilder {
        key: Arc<str>,
        token: u8,
        binds: Arc<std::sync::Mutex<Vec<std::net::SocketAddr>>>,
    }
    impl Build for BindingBuilder {
        type ConnHandler = TokenConnHandler;
        type Server = BindingServer;
        type Err = std::io::Error;
        async fn build_server(self) -> Result<Self::Server, Self::Err> {
            let listener = tokio::net::TcpListener::bind(self.key.as_ref()).await?;
            self.binds.lock().unwrap().push(listener.local_addr()?);
            Ok(BindingServer {
                listener,
                token: self.token,
            })
        }
        fn build_conn_handler(self) -> Result<Self::ConnHandler, Self::Err> {
            Ok(TokenConnHandler(self.token))
        }
        fn key(&self) -> &Arc<str> {
            &self.key
        }
    }

    /// The token `addr` serves, or `None` if nothing answers there. Every wait
    /// is bounded, so a listener that is bound but not served is read as no
    /// answer instead of hanging the test.
    async fn served_token(addr: std::net::SocketAddr) -> Option<u8> {
        use tokio::io::AsyncReadExt;
        let wait = std::time::Duration::from_secs(2);
        let mut stream = tokio::time::timeout(wait, tokio::net::TcpStream::connect(addr))
            .await
            .ok()?
            .ok()?;
        let mut token = [0u8; 1];
        tokio::time::timeout(wait, stream.read_exact(&mut token))
            .await
            .ok()?
            .ok()?;
        Some(token[0])
    }

    /// The address a client reaches a wildcard bind on.
    fn reachable(addr: std::net::SocketAddr) -> std::net::SocketAddr {
        match addr {
            std::net::SocketAddr::V4(v4) if v4.ip().is_unspecified() => {
                std::net::SocketAddr::from(([127, 0, 0, 1], v4.port()))
            }
            other => other,
        }
    }

    /// A configuration that names one listener twice is refused before
    /// anything is bound. The key is the listener's identity, so the second
    /// entry can only shadow the first, and the first's socket would be bound
    /// and dropped while the log says it is listening.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_configuration_that_names_one_key_twice_is_refused_before_binding() {
        let binds = Arc::new(std::sync::Mutex::new(Vec::new()));
        let loader = Loader::new();
        let builder = |token: u8| BindingBuilder {
            key: "127.0.0.1:0".into(),
            token,
            binds: Arc::clone(&binds),
        };
        let error = match loader
            .snapshot()
            .prepare::<BindingServer, BindingBuilder>(vec![builder(1), builder(2)])
            .await
        {
            Ok(_) => panic!("a configuration that names one key twice must be refused"),
            Err(error) => error,
        };
        assert!(
            error
                .to_string()
                .contains("duplicate listener configuration key `127.0.0.1:0`"),
            "the refusal must name the key to drop; got: {error}"
        );
        let bound = binds.lock().unwrap().clone();
        assert!(
            bound.is_empty(),
            "a refused configuration must bind nothing: {bound:?}"
        );
        assert!(
            loader.handles.is_empty(),
            "a refused configuration must install no listener handle"
        );
    }

    /// Distinct keys are distinct listeners even when one of them is a
    /// wildcard: each binds and serves its own token, so the refusal above is
    /// keyed on the identity and not on the number of listeners.
    #[tokio::test(flavor = "multi_thread")]
    async fn distinct_keys_each_bind_and_serve_their_own_token() {
        let binds = Arc::new(std::sync::Mutex::new(Vec::new()));
        let mut loader = Loader::new();
        let mut join_set = tokio::task::JoinSet::new();
        let builder = |key: &str, token: u8| BindingBuilder {
            key: key.into(),
            token,
            binds: Arc::clone(&binds),
        };
        let prepared = loader
            .snapshot()
            .prepare::<BindingServer, BindingBuilder>(vec![
                builder("127.0.0.1:0", 1),
                builder("0.0.0.0:0", 2),
            ])
            .await
            .expect("distinct keys are two listeners");
        loader.commit(&mut join_set, prepared).unwrap();
        assert_eq!(loader.handles.len(), 2);
        let bound = binds.lock().unwrap().clone();
        assert_eq!(bound.len(), 2, "both listeners bound: {bound:?}");
        assert_eq!(
            served_token(reachable(bound[0])).await,
            Some(1),
            "the first address serves the first listener's token at {}",
            bound[0]
        );
        assert_eq!(
            served_token(reachable(bound[1])).await,
            Some(2),
            "the second address serves the second listener's token at {}",
            bound[1]
        );
        join_set.shutdown().await;
    }

    #[tokio::test]
    async fn a_replacement_bumps_the_generation_observably() {
        let reloadable = ReloadableHandler::new(NoopConnHandler);
        let mut generation = reloadable.generation();
        assert_eq!(*generation.borrow(), 0);

        reloadable.replace(Arc::new(NoopConnHandler));
        // Subscribed before the replacement, the receiver resolves the moment
        // it is applied — deterministic acknowledgement, no sleep.
        generation.changed().await.unwrap();
        assert_eq!(*generation.borrow(), 1);

        // A second replacement bumps again.
        reloadable.replace(Arc::new(NoopConnHandler));
        generation.changed().await.unwrap();
        assert_eq!(*generation.borrow(), 2);
    }
}
