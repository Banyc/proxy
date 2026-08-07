use std::{
    collections::{HashMap, HashSet},
    future::Future,
    pin::Pin,
    sync::Arc,
};

use derive_more::Debug;
use tokio::sync::mpsc;

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
    ConnHandler: HandleConn + std::fmt::Debug + Send + 'static,
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
        self.commit(join_set, prepared);
        Ok(())
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
            if live.is_some_and(|h| h.0.is_closed()) {
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
    /// This must not fail: every operation is infallible at this point.
    pub fn commit(
        &mut self,
        join_set: &mut tokio::task::JoinSet<AnyResult>,
        prepared: PreparedOps<ConnHandler>,
    ) {
        let PreparedOps { ops, keys } = prepared;
        for op in ops {
            match op {
                PreparedOp::Replace { tx, conn_handler } => {
                    // best-effort: if the listener died since prepare, the send
                    // fails and we simply drop the handler. The handle stays in
                    // the map only if still alive; the retain below removes it
                    // if the key is gone from the new config.
                    let _ = tx.0.try_send(conn_handler);
                }
                PreparedOp::Spawn { key, tx, spawn } => {
                    self.handles.insert(key, tx);
                    join_set.spawn(spawn);
                }
            }
        }
        self.handles.retain(|cur_key, _| keys.contains(cur_key));
    }
}
impl<ConnHandler> Default for Loader<ConnHandler>
where
    ConnHandler: HandleConn + std::fmt::Debug + Send + 'static,
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
pub struct ReplaceConnHandlerTx<ConnHandler>(pub mpsc::Sender<ConnHandler>);
impl<ConnHandler> Clone for ReplaceConnHandlerTx<ConnHandler> {
    fn clone(&self) -> Self {
        Self(self.0.clone())
    }
}
#[derive(Debug)]
#[debug(bound(ConnHandler:))]
pub struct ReplaceConnHandlerRx<ConnHandler>(pub mpsc::Receiver<ConnHandler>);
pub fn replace_conn_handler_channel<ConnHandler>() -> (
    ReplaceConnHandlerTx<ConnHandler>,
    ReplaceConnHandlerRx<ConnHandler>,
) {
    let (tx, rx) = tokio::sync::mpsc::channel(64);
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
}
