use std::sync::Arc;

use common::error::AnyError;
use file_watcher_tokio::EventActor;

pub mod multi_file_config;
pub mod toml;

pub trait ConfigReader {
    type Config;
    fn read_config(
        &self,
    ) -> impl std::future::Future<Output = Result<Self::Config, AnyError>> + Send;
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
impl EventActor for ConfigWatcher {
    async fn notify(&self, _event: notify::Event) {
        self.tx.notify_one();
    }
}

pub fn spawn_watch_tasks(config_file_paths: &[Arc<str>]) -> Arc<tokio::sync::Notify> {
    let watcher = ConfigWatcher::new();
    let notify_rx = Arc::clone(watcher.notify_rx());
    let watcher = Arc::new(watcher);
    config_file_paths.iter().cloned().for_each(|path| {
        let watcher = Arc::clone(&watcher);
        tokio::spawn(async move { file_watcher_tokio::watch_file(path.as_ref(), watcher).await });
    });
    notify_rx
}
