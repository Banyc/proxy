use std::{
    collections::{HashMap, HashSet},
    fmt::Debug,
    sync::Arc,
};

use tokio::sync::mpsc;

use crate::error::AnyResult;

/// A listener loader that spawns and kills listeners
#[derive(Debug)]
pub struct Loader<ConnHandler> {
    /// Handles of the listeners using the actor model pattern
    handles: HashMap<Arc<str>, mpsc::Sender<ConnHandler>>,
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
        // Spawn servers
        let mut keys = HashSet::new();
        for server in builders {
            keys.insert(server.key().to_owned());

            // Hot reloading
            if let Some(handle) = self.handles.get(server.key()) {
                let conn_handler = server.build_conn_handler()?;
                handle.send(conn_handler).await.unwrap();
                continue;
            }

            // Spawn server
            let key = server.key().to_owned();
            let (set_conn_handler_tx, set_conn_handler_rx) = tokio::sync::mpsc::channel(64);
            let server = server.build_server().await?;
            self.handles.insert(key, set_conn_handler_tx);
            join_set.spawn(async move {
                server.serve(set_conn_handler_rx).await?;
                Ok(())
            });
        }

        // Remove servers
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
        set_conn_handler_rx: tokio::sync::mpsc::Receiver<Self::ConnHandler>,
    ) -> impl Future<Output = AnyResult> + Send;
}
