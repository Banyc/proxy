use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
};

use derive_more::Debug;
use tokio::sync::mpsc;

use crate::error::AnyResult;

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
        let mut keys = HashSet::new();
        for server in builders {
            let key = server.key().to_owned();
            keys.insert(key.clone());
            if self.handles.get(&key).is_some_and(|h| h.0.is_closed()) {
                self.handles.remove(&key);
            }
            if let Some(handle) = self.handles.get(&key) {
                let conn_handler = server.build_conn_handler()?;
                if handle.0.send(conn_handler).await.is_err() {
                    self.handles.remove(&key);
                }
                continue;
            }
            let (set_conn_handler_tx, set_conn_handler_rx) = replace_conn_handler_channel();
            let server = server.build_server().await?;
            self.handles.insert(key, set_conn_handler_tx);
            join_set.spawn(async move {
                server.serve(set_conn_handler_rx).await?;
                Ok(())
            });
        }
        self.handles.retain(|cur_key, _| keys.contains(cur_key));
        Ok(())
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
}
