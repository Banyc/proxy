use std::{
    collections::{HashMap, HashSet},
    future::Future,
    pin::Pin,
    sync::Arc,
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
        let prepared = self.prepare(builders).await?;
        self.commit(join_set, prepared)
    }

    /// Resolve and bind every listener in `builders` against the live
    /// `handles` without mutating live state.
    ///
    /// For an existing live listener a [`PreparedOp::Replace`] is produced
    /// carrying the freshly-built handler to send over the existing channel.
    /// For a new listener a [`PreparedOp::Spawn`] is produced carrying the
    /// bound `Server` and a fresh `ReplaceConnHandlerTx`; the server task is
    /// *not* spawned yet.
    ///
    /// Dropping the returned [`PreparedOps`] without [`Self::commit`] simply
    /// drops the bound servers and handlers — no live state is touched and no
    /// task is spawned.
    pub async fn prepare<Server, Builder>(
        &self,
        builders: Vec<Builder>,
    ) -> Result<PreparedOps<ConnHandler>, AnyError>
    where
        Server: Serve<ConnHandler = ConnHandler> + Send + 'static,
        Builder: Build<ConnHandler = ConnHandler, Server = Server>,
    {
        let mut keys = HashSet::new();
        let mut ops = Vec::with_capacity(builders.len());
        for builder in builders {
            let key = builder.key().to_owned();
            keys.insert(key.clone());
            let live = self.handles.get(&key);
            if live.is_some_and(|h| h.is_closed()) {
                // dead listener — treat as new so it is re-spawned below
            } else if let Some(handle) = live {
                let conn_handler = builder.build_conn_handler()?;
                ops.push(PreparedOp::Replace {
                    tx: handle.clone(),
                    conn_handler,
                });
                continue;
            }
            let (set_conn_handler_tx, set_conn_handler_rx) = replace_conn_handler_channel();
            let server = builder.build_server().await?;
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

    /// Atomically apply a prepared reload: send new handlers to existing
    /// listeners, spawn new listener tasks, install new handles, and drop
    /// handles for listeners that are no longer in the config.
    ///
    /// Returns an error if a listener died between [`Self::prepare`] and
    /// commit, so the handler update is *never silently lost*. The caller is
    /// responsible for surfacing the failure (the global state may already
    /// have been swapped, but the lost update is reported rather than
    /// swallowed).
    pub fn commit(
        &mut self,
        join_set: &mut tokio::task::JoinSet<AnyResult>,
        prepared: PreparedOps<ConnHandler>,
    ) -> Result<(), AnyError> {
        let PreparedOps { ops, keys } = prepared;
        for op in ops {
            match op {
                PreparedOp::Replace { tx, conn_handler } => {
                    // A closed receiver means the listener died since prepare.
                    // The handler update cannot be delivered — fail loudly
                    // rather than silently dropping it.
                    tx.send(conn_handler)
                        .map_err(|_| AnyError::from("listener died before reload commit"))?;
                }
                PreparedOp::Spawn { key, tx, spawn } => {
                    self.handles.insert(key, tx);
                    join_set.spawn(spawn);
                }
            }
        }
        self.handles.retain(|cur_key, _| keys.contains(cur_key));
        Ok(())
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
    /// Hot-swap the handler of an existing live listener.
    Replace {
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
}
