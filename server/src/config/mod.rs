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
        let watched = Arc::clone(&path);
        // One watcher thread, at most one terminal exit to report.
        const WATCHER_EXIT_CAPACITY: usize = 1;
        let (failure_tx, mut failure_rx) =
            tokio::sync::mpsc::channel::<RootTaskExit>(WATCHER_EXIT_CAPACITY);
        // A thread-spawn failure must still be observable: the moved-in sender
        // is dropped with the failed closure, so keep a clone to report it.
        let report_spawn_failure = failure_tx.clone();
        let thread = std::thread::Builder::new()
            .name("config_watcher".to_owned())
            .spawn(move || {
                let _ = failure_tx.try_send(run_watch_thread(path, watcher));
            });
        if let Err(error) = thread {
            let _ = report_spawn_failure.try_send(watcher_failure(&watched, error));
        }
        process_tasks.spawn(async move {
            failure_rx.recv().await.unwrap_or(RootTaskExit::Completed {
                task: "config_watcher",
            })
        });
    });
    signal
}

/// Run one watcher on a dedicated, detached OS thread for the rest of the
/// process's life.
///
/// `file_watcher_tokio`'s FSEvents callback does
/// `tx.blocking_send(res).unwrap()` inside a C callback. If the `watch_file`
/// future is dropped (closing its receiver) while a delivery is in flight, the
/// failed send panics inside that callback, which cannot unwind, and aborts the
/// process. `watch_file` owns both the receiver and the watcher, so no caller
/// can order the watcher's shutdown before the receiver's; the only safe
/// ownership is to never drop the future. The thread is therefore detached and
/// its runtime outlives every abort/reap performed on `process_tasks`; process
/// exit terminates it with the receiver still open, so a late callback can
/// never observe a closed channel.
fn run_watch_thread(path: Arc<str>, watcher: ConfigWatcher) -> RootTaskExit {
    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => return watcher_failure(path.as_ref(), error),
    };
    let outcome = runtime.block_on(file_watcher_tokio::watch_file(path.as_ref(), watcher));
    // Reached only when the watch ends on its own; on the healthy path the
    // thread parks in `watch_file` until process exit drops nothing.
    match outcome {
        Ok(()) => RootTaskExit::Completed {
            task: "config_watcher",
        },
        Err(error) => watcher_failure(path.as_ref(), error),
    }
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

    #[tokio::test]
    async fn a_missing_config_file_surfaces_as_a_fatal_watcher_exit() {
        let path: Arc<str> = Arc::from("/nonexistent/config-does-not-exist.toml");
        let mut process_tasks: tokio::task::JoinSet<RootTaskExit> = tokio::task::JoinSet::new();
        let _signal = spawn_watch_tasks(&mut process_tasks, std::slice::from_ref(&path));
        let joined =
            tokio::time::timeout(std::time::Duration::from_secs(5), process_tasks.join_next())
                .await
                .expect("a failed watcher must surface instead of parking")
                .expect("the task set must yield the coordinator")
                .expect("the coordinator must not be cancelled");
        match joined {
            RootTaskExit::Failed { task, detail } => {
                assert_eq!(task, "config_watcher");
                assert!(detail.contains("config-does-not-exist.toml"), "{detail}");
            }
            RootTaskExit::Completed { .. } => panic!("a missing file must be fatal"),
        }
    }
}

#[derive(Debug, Clone)]
pub struct ConfigChangeSignal(pub Notify);
