use std::{
    collections::{HashMap, HashSet},
    fmt::Debug,
    sync::Arc,
};

use tokio::sync::mpsc;

use crate::error::AnyResult;

#[derive(Debug)]
pub struct Loader<H> {
    handles: HashMap<Arc<str>, mpsc::Sender<H>>,
}

impl<H> Loader<H>
where
    H: Hook + std::fmt::Debug,
{
    pub fn new() -> Self {
        Self {
            handles: HashMap::new(),
        }
    }

    pub async fn load_and_clean<S, B>(
        &mut self,
        join_set: &mut tokio::task::JoinSet<AnyResult>,
        builders: Vec<B>,
    ) -> AnyResult
    where
        S: Server<Hook = H> + Send + 'static,
        B: Builder<Hook = H, Server = S>,
    {
        // Spawn servers
        let mut keys = HashSet::new();
        for server in builders {
            keys.insert(server.key().to_owned());

            // Hot reloading
            if let Some(handle) = self.handles.get(server.key()) {
                let hook = server.build_hook()?;
                handle.send(hook).await.unwrap();
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

impl<H> Default for Loader<H>
where
    H: Hook + std::fmt::Debug,
{
    fn default() -> Self {
        Self::new()
    }
}

pub trait Hook {}

pub trait Builder {
    type Hook: Hook;
    type Server: Server<Hook = Self::Hook>;
    type Err: std::error::Error + Send + Sync + 'static;
    fn build_server(
        self,
    ) -> impl std::future::Future<Output = Result<Self::Server, Self::Err>> + Send;
    fn build_hook(self) -> Result<Self::Hook, Self::Err>;
    fn key(&self) -> &Arc<str>;
}

pub trait Server {
    type Hook: Hook;
    fn handle(&self) -> mpsc::Sender<Self::Hook>;
    /// Server ends if the caller does not hold a handle to the server.
    fn serve(self) -> impl std::future::Future<Output = AnyResult> + Send;
}
