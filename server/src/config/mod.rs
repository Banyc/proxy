use std::sync::Arc;

use common::{error::AnyError, lifecycle::process::RootTaskExit, notify::Notify};

pub mod multi_file_config;
pub mod toml;

pub trait ReadConfig {
    type Config;
    fn read_config(&self) -> impl Future<Output = Result<Self::Config, AnyError>> + Send;
}

#[derive(Debug, Clone)]
pub struct ConfigWatcher {
    signal: ConfigChangeSignal,
}
impl ConfigWatcher {
    pub fn new() -> Self {
        let signal = ConfigChangeSignal(Notify::new());
        Self { signal }
    }

    pub fn signal(&self) -> &ConfigChangeSignal {
        &self.signal
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
        self.signal.0.notify_waiters();
    }
}

pub fn spawn_watch_tasks(
    process_tasks: &mut tokio::task::JoinSet<RootTaskExit>,
    config_file_paths: &[Arc<str>],
) -> ConfigChangeSignal {
    let watcher = ConfigWatcher::new();
    let signal = watcher.signal().clone();
    config_file_paths.iter().for_each(|path| {
        let watcher = watcher.clone();
        let path = path.clone();
        process_tasks.spawn(async move {
            let watched = Arc::clone(&path);
            match file_watcher_tokio::watch_file(path.as_ref(), watcher).await {
                Ok(()) => RootTaskExit::Completed {
                    task: "config_watcher",
                },
                Err(error) => watcher_failure(&watched, error),
            }
        });
    });
    signal
}

fn watcher_failure(path: &str, error: impl std::fmt::Display) -> RootTaskExit {
    RootTaskExit::Failed {
        task: "config_watcher",
        detail: format!("{path}:{error}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn watcher_failure_reports_the_path_and_the_error() {
        let exit = watcher_failure(
            "/tmp/missing-config",
            std::io::Error::other("synthetic watcher failure"),
        );
        match exit {
            RootTaskExit::Failed { task, detail } => {
                assert_eq!(task, "config_watcher");
                assert!(detail.contains("/tmp/missing-config"), "{detail}");
                assert!(detail.contains("synthetic watcher failure"), "{detail}");
            }
            RootTaskExit::Completed { .. } => panic!("expected a failure"),
        }
    }
}

#[derive(Debug, Clone)]
pub struct ConfigChangeSignal(pub Notify);
