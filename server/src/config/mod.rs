use std::sync::Arc;

use async_trait::async_trait;
use common::error::AnyError;
use file_watcher_tokio::EventActor;

pub mod multi_file_config;

#[async_trait]
pub trait ConfigReader {
    type Config;
    async fn read_config(&self) -> Result<Self::Config, AnyError>;
}

pub struct ConfigWatcher {
    tx: Arc<tokio::sync::Notify>,
}

impl ConfigWatcher {
    pub fn new() -> Self {
        let tx = Arc::new(tokio::sync::Notify::new());
        Self { tx }
    }

    pub fn notify_rx(&self) -> &Arc<tokio::sync::Notify> {
        &self.tx
    }
}

impl Default for ConfigWatcher {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl EventActor for ConfigWatcher {
    async fn notify(&self, _event: notify::Event) {
        self.tx.notify_one();
    }
}
