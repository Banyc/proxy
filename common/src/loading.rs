use std::{
    collections::{HashMap, HashSet},
    io,
    sync::Arc,
};

use async_trait::async_trait;
use tokio::sync::mpsc;

use crate::error::AnyResult;

#[derive(Debug)]
pub struct Loader<H> {
    set_hook_tx: HashMap<Arc<str>, mpsc::Sender<H>>,
}

impl<H> Loader<H>
where
    H: Hook + std::fmt::Debug,
{
    pub fn new() -> Self {
        Self {
            set_hook_tx: HashMap::new(),
        }
    }

    pub async fn load<S, B>(
        &mut self,
        join_set: &mut tokio::task::JoinSet<AnyResult>,
        builders: Vec<B>,
    ) -> io::Result<()>
    where
        S: Server<Hook = H> + Send + 'static,
        B: Builder<Hook = H, Server = S>,
    {
        // Spawn servers
        let mut keys = HashSet::new();
        for server in builders {
            keys.insert(server.key().to_owned());

            // Hot reloading
            if let Some(set_hook_tx) = self.set_hook_tx.get(server.key()) {
                let hook = server.build_hook()?;
                set_hook_tx.send(hook).await.unwrap();
                continue;
            }

            // Spawn server
            let key = server.key().to_owned();
            let server = server.build_server().await?;
            self.set_hook_tx.insert(key, server.set_hook_tx().clone());
            join_set.spawn(async move {
                server.serve().await?;
                Ok(())
            });
        }

        // Remove servers
        let mut remove_setters = Vec::new();
        self.set_hook_tx.iter().for_each(|(cur_key, _)| {
            if !keys.contains(cur_key) {
                remove_setters.push(cur_key.clone());
            }
        });
        for cur_key in remove_setters {
            self.set_hook_tx.remove(&cur_key);
        }
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

#[async_trait]
pub trait Builder {
    type Hook: Hook;
    type Server: Server<Hook = Self::Hook>;
    async fn build_server(self) -> io::Result<Self::Server>;
    fn build_hook(self) -> io::Result<Self::Hook>;
    fn key(&self) -> &Arc<str>;
}

#[async_trait]
pub trait Server {
    type Hook: Hook;
    fn set_hook_tx(&self) -> &mpsc::Sender<Self::Hook>;
    async fn serve(self) -> AnyResult;
}
