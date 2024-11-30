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
    ConnHandler: HandleConn + std::fmt::Debug,
{
    pub fn new() -> Self {
        Self {
            handles: HashMap::new(),
        }
    }

    pub async fn spawn_and_clean<S, B>(
        &mut self,
        join_set: &mut tokio::task::JoinSet<AnyResult>,
        builders: Vec<B>,
    ) -> AnyResult
    where
        S: Serve<ConnHandler = ConnHandler> + Send + 'static,
        B: Build<ConnHandler = ConnHandler, Server = S>,
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
            let server = server.build_server().await?;
            self.handles.insert(key, server.handle());
            join_set.spawn(async move {
                server.serve().await?;
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
    ConnHandler: HandleConn + std::fmt::Debug,
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
    fn build_server(
        self,
    ) -> impl std::future::Future<Output = Result<Self::Server, Self::Err>> + Send;
    fn build_conn_handler(self) -> Result<Self::ConnHandler, Self::Err>;
    fn key(&self) -> &Arc<str>;
}

/// A listener including the business logic for the accepted connections
pub trait Serve {
    type ConnHandler: HandleConn;
    /// The handle of this server using the actor model
    ///
    /// If all the handle has been dropped, the listener must despawn eventually but still keep all its connections alive.
    fn handle(&self) -> mpsc::Sender<Self::ConnHandler>;
    /// Server ends if the caller does not hold a handle to the server.
    fn serve(self) -> impl std::future::Future<Output = AnyResult> + Send;
}
