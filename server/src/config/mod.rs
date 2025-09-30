use std::sync::Arc;

use common::{error::AnyError, notify::Notify};

pub mod multi_file_config;
pub mod toml;

pub trait ReadConfig {
    type Config;
    fn read_config(&self) -> impl Future<Output = Result<Self::Config, AnyError>> + Send;
}

#[derive(Debug, Clone)]
pub struct ConfigWatcher {
    tx: ConfigChanged,
}
impl ConfigWatcher {
    pub fn new() -> Self {
        let tx = ConfigChanged(Notify::new());
        Self { tx }
    }

    pub fn notify(&self) -> &ConfigChanged {
        &self.tx
    }
}
impl Default for ConfigWatcher {
    fn default() -> Self {
        Self::new()
    }
}
impl file_watcher_tokio::HandleEvent for ConfigWatcher {
    async fn handle_event(&mut self, event: file_watcher_tokio::Event) {
        let may_changed =
            event.kind.is_create() || event.kind.is_modify() || event.kind.is_remove();
        if !may_changed {
            return;
        }
        self.tx.0.notify_waiters();
    }
}

pub fn spawn_watch_tasks(config_file_paths: &[Arc<str>]) -> ConfigChanged {
    let watcher = ConfigWatcher::new();
    let notify = watcher.notify().clone();
    config_file_paths.iter().cloned().for_each(|path| {
        let watcher = watcher.clone();
        tokio::spawn(async move { file_watcher_tokio::watch_file(path.as_ref(), watcher).await });
    });
    notify
}

#[derive(Debug, Clone)]
pub struct ConfigChanged(pub Notify);
