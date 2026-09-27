use std::sync::Arc;

use common::{
    error::AnyError,
    lifecycle::process::RootTaskExit,
    notify::{Notify, Subscription},
};

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
        let signal = ConfigChangeSignal::new();
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
        self.signal.notify_waiters();
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
///
/// A watch whose path is **absent** is re-armed rather than reported. `notify`
/// cannot establish a watch on a path that is not there, and a config path is
/// legitimately absent for a moment: a deployment that removes the file and
/// recreates it, an atomic rename racing the arming, an operator editing it.
/// `main` treats a failed root task as fatal, and by the time this thread runs
/// a valid configuration has already been read and bound, so reporting the
/// absence would shut the process down and drop every live session over a path
/// that is about to come back. Any other failure — one a retry cannot fix — is
/// still reported, and is what makes a process with a dead watcher visible to
/// its supervisor.
fn run_watch_thread(path: Arc<str>, watcher: ConfigWatcher) -> RootTaskExit {
    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => return watcher_failure(path.as_ref(), error),
    };
    loop {
        let outcome = runtime.block_on(file_watcher_tokio::watch_file(
            path.as_ref(),
            watcher.clone(),
        ));
        // Reached only when the watch ends on its own; on the healthy path the
        // thread parks in `watch_file` until process exit drops nothing.
        match outcome {
            Ok(()) => {
                return RootTaskExit::Completed {
                    task: "config_watcher",
                };
            }
            Err(error) => {
                if std::path::Path::new(path.as_ref()).exists() {
                    return watcher_failure(path.as_ref(), error);
                }
                tracing::warn!(
                    path = path.as_ref(),
                    %error,
                    "Config path is absent; re-arming the watcher"
                );
                std::thread::sleep(WATCH_REARM_INTERVAL);
            }
        }
    }
}

/// How long to wait before trying again to watch a path that is absent.
const WATCH_REARM_INTERVAL: std::time::Duration = std::time::Duration::from_millis(100);

fn watcher_failure(path: &str, error: impl std::fmt::Display) -> RootTaskExit {
    RootTaskExit::Failed {
        task: "config_watcher",
        detail: format!("{path}:{error}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A filesystem event that is neither a create, a modify, nor a remove
    /// must not signal a reload: the watcher's classification is what keeps
    /// unrelated events (notify's catch-all `Any` kind, access events) from
    /// rebuilding a config generation.
    #[tokio::test(start_paused = true)]
    async fn an_event_that_is_not_create_modify_or_remove_does_not_signal() {
        use file_watcher_tokio::{Event, HandleEvent};

        let mut watcher = ConfigWatcher::new();
        let mut subscription = watcher.signal().subscription();

        // `notify::Event::default()` carries the catch-all `EventKind::Any`.
        watcher.handle_event(Event::default()).await;

        assert!(
            tokio::time::timeout(
                std::time::Duration::from_secs(3600),
                subscription.notified()
            )
            .await
            .is_err(),
            "an event that is not a create, modify, or remove must not signal a reload"
        );

        // Control: the probe above does observe a real broadcast, so the
        // assertion is about the classification, not about a dead channel.
        watcher.signal().notify_waiters();
        tokio::time::timeout(
            std::time::Duration::from_secs(3600),
            subscription.notified(),
        )
        .await
        .expect("the probe must observe a broadcast");
    }

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

    /// A config path that is **absent when the watch is established** must not
    /// be reported as a terminal watcher failure, and the watcher must re-arm
    /// once the path exists.
    ///
    /// `notify` cannot watch a path that is not there, and a config path is
    /// legitimately absent for a moment — a deployment that removes the file
    /// and recreates it, an atomic rename racing the arming, an operator
    /// editing it. `main` treats a failed root task as fatal, and a valid
    /// configuration has already been read and bound by the time this thread
    /// runs, so reporting the absence would shut the process down and drop
    /// every live session over a path that is about to come back. Both
    /// directions are observed here: no terminal exit within a bounded window
    /// while the path is absent, and a change delivered after the path exists.
    #[tokio::test]
    async fn a_config_path_that_is_absent_at_arming_is_re_armed_not_reported_as_fatal() {
        let dir = std::env::temp_dir().join(format!(
            "proxy-watch-absent-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("config.toml");
        let path: Arc<str> = Arc::from(file.to_str().unwrap());

        let mut process_tasks: tokio::task::JoinSet<RootTaskExit> = tokio::task::JoinSet::new();
        let signal = spawn_watch_tasks(&mut process_tasks, std::slice::from_ref(&path));
        let mut subscription = signal.subscription();

        // The path does not exist yet: the watcher must retry rather than
        // report a terminal exit, which `main` would treat as a shutdown.
        assert!(
            tokio::time::timeout(std::time::Duration::from_secs(2), process_tasks.join_next())
                .await
                .is_err(),
            "a watcher whose path is absent must re-arm, not report a terminal exit that shuts \
             the serving process down"
        );

        // Create the path and keep editing it; the watcher must arm on it and
        // deliver a change. The notification is the success condition and each
        // attempt is bounded, so a watcher that never re-arms fails instead of
        // hanging.
        let mut notified = false;
        for attempt in 0..20 {
            std::fs::write(&file, format!("change = {attempt}\n")).unwrap();
            if tokio::time::timeout(
                std::time::Duration::from_millis(500),
                subscription.notified(),
            )
            .await
            .is_ok()
            {
                notified = true;
                break;
            }
        }
        assert!(
            notified,
            "the watcher must re-arm on the path once it exists: no change to {path} signalled \
             after the path was recreated"
        );
        assert!(
            tokio::time::timeout(
                std::time::Duration::from_millis(100),
                process_tasks.join_next()
            )
            .await
            .is_err(),
            "a re-armed watcher must not report a terminal exit"
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    /// Every configured config file must get its own watcher. The serve loop
    /// merges all of them, so a file whose edits are not watched is edited with
    /// no reload and no error — the operator's change is silently not applied.
    ///
    /// The three files sit in **different directories**, so no single watch can
    /// cover them: a signal after a write to each file can only come from a
    /// watcher on that file's own path. A fresh subscription per file keeps the
    /// notifications attributable to that file's write.
    #[test]
    fn every_configured_config_file_gets_its_own_watcher() {
        let root = std::env::temp_dir().join(format!(
            "proxy-watch-paths-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let paths: Vec<Arc<str>> = ["a", "b", "c"]
            .iter()
            .map(|name| {
                let dir = root.join(name);
                std::fs::create_dir_all(&dir).unwrap();
                let file = dir.join("config.toml");
                std::fs::write(&file, "initial = 1\n").unwrap();
                Arc::<str>::from(file.to_str().unwrap())
            })
            .collect();

        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async {
            let mut process_tasks: tokio::task::JoinSet<RootTaskExit> = tokio::task::JoinSet::new();
            let signal = spawn_watch_tasks(&mut process_tasks, &paths);
            assert_eq!(
                process_tasks.len(),
                paths.len(),
                "every configured config file must have its own watcher"
            );
            for (index, path) in paths.iter().enumerate() {
                // A subscription created now does not observe earlier
                // broadcasts, so the wakeup below belongs to this file's write.
                let mut subscription = signal.subscription();
                let mut notified = false;
                for attempt in 0..20 {
                    std::fs::write(path.as_ref(), format!("change = {index}-{attempt}\n")).unwrap();
                    if tokio::time::timeout(
                        std::time::Duration::from_millis(500),
                        subscription.notified(),
                    )
                    .await
                    .is_ok()
                    {
                        notified = true;
                        break;
                    }
                }
                assert!(
                    notified,
                    "no watcher signalled for {path}: a configured config file with no watcher is \
                     edited with no reload and no error"
                );
            }
        });
        drop(runtime);
        std::fs::remove_dir_all(&root).ok();
    }

    /// Every event kind that can edit a config file's contents must signal a
    /// reload: the classification in [`ConfigWatcher::handle_event`] is the
    /// only thing that turns a filesystem change into the reload the operator
    /// asked for, and the negative half of that classification is pinned by
    /// `an_event_that_is_not_create_modify_or_remove_does_not_signal`.
    ///
    /// The events are captured from the platform's own watcher rather than
    /// constructed, because constructing a `notify::EventKind` would need a
    /// `notify` dependency this crate does not declare. Each captured kind is
    /// replayed through `handle_event` against a fresh subscription, so each
    /// assertion is about that kind's own classification rather than about a
    /// signal some other kind produced — which matters, because on this
    /// platform a single write is reported as several kinds at once and an
    /// end-to-end test through `spawn_watch_tasks` therefore cannot tell them
    /// apart.
    #[test]
    fn every_change_kind_that_can_edit_a_config_file_signals_a_reload() {
        use file_watcher_tokio::HandleEvent;

        let captured = capture_change_events();
        /// One event kind to replay, under the label an assertion failure names.
        type JudgedKind = (&'static str, fn(&file_watcher_tokio::Event) -> bool);
        let kinds: [JudgedKind; 3] = [
            ("a create", |event| event.kind.is_create()),
            ("a modify", |event| event.kind.is_modify()),
            ("a remove", |event| event.kind.is_remove()),
        ];
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("the replay runtime builds");
        runtime.block_on(async {
            let mut watcher = ConfigWatcher::new();
            for (label, is_kind) in kinds {
                let event = captured
                    .iter()
                    .find(|event| is_kind(event))
                    .unwrap_or_else(|| {
                        panic!(
                            "the platform's watcher delivered no {label} for edits to a watched \
                         file, so its classification cannot be probed; captured kinds: {:?}",
                            captured
                                .iter()
                                .map(|event| format!("{:?}", event.kind))
                                .collect::<Vec<_>>()
                        )
                    });
                let mut subscription = watcher.signal().subscription();
                watcher.handle_event(event.clone()).await;
                assert!(
                    tokio::time::timeout(
                        std::time::Duration::from_secs(5),
                        subscription.notified()
                    )
                    .await
                    .is_ok(),
                    "{label} of a config file must signal a reload: a kind the watcher discards \
                     is an edit the server never applies"
                );
            }
        });
    }

    /// Capture real events from the platform's watcher for the changes that
    /// can edit a config file: a write over the file (which this platform
    /// reports as a create and a modify) and a removal (a remove). Bounded, so
    /// a host whose watcher delivers none of them fails instead of hanging.
    fn capture_change_events() -> Vec<file_watcher_tokio::Event> {
        struct Recorder(Arc<std::sync::Mutex<Vec<file_watcher_tokio::Event>>>);
        impl file_watcher_tokio::HandleEvent for Recorder {
            async fn handle_event(&mut self, event: file_watcher_tokio::Event) {
                self.0.lock().unwrap().push(event);
            }
        }

        let dir = std::env::temp_dir().join(format!(
            "proxy-watch-kinds-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.toml");
        std::fs::write(&path, "initial = 1\n").unwrap();
        let watched: Arc<str> = Arc::from(path.to_str().unwrap());
        let seen: Arc<std::sync::Mutex<Vec<file_watcher_tokio::Event>>> =
            Arc::new(std::sync::Mutex::new(Vec::new()));

        // The watcher future owns the receiver the platform's callback writes
        // to, and dropping it while a delivery is in flight aborts the process
        // (see `run_watch_thread`), so this one lives on a detached thread for
        // the rest of the test binary's life.
        std::thread::Builder::new()
            .name("config_kinds_probe".to_owned())
            .spawn({
                let seen = Arc::clone(&seen);
                move || {
                    let runtime = tokio::runtime::Builder::new_current_thread()
                        .enable_all()
                        .build()
                        .expect("the probe runtime builds");
                    let _ = runtime.block_on(file_watcher_tokio::watch_file(
                        watched.as_ref(),
                        Recorder(seen),
                    ));
                }
            })
            .expect("the probe thread spawns");

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
        loop {
            std::fs::write(&path, "edited = 1\n").unwrap();
            std::thread::sleep(std::time::Duration::from_millis(50));
            std::fs::remove_file(&path).unwrap();
            std::thread::sleep(std::time::Duration::from_millis(50));

            let captured = seen.lock().unwrap();
            if captured.iter().any(|event| event.kind.is_create())
                && captured.iter().any(|event| event.kind.is_modify())
                && captured.iter().any(|event| event.kind.is_remove())
            {
                let events: Vec<file_watcher_tokio::Event> = captured.clone();
                drop(captured);
                std::fs::remove_dir_all(&dir).ok();
                return events;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "the platform's watcher delivered no create, modify, and remove for edits to a \
                 watched file within the budget, so the classification cannot be probed; \
                 captured kinds: {:?}",
                captured
                    .iter()
                    .map(|event| format!("{:?}", event.kind))
                    .collect::<Vec<_>>()
            );
            drop(captured);
        }
    }
}

/// The broadcast the serve loop reloads on.
///
/// The signal hands out the two capabilities its consumers need — a
/// [`Subscription`] to await a change, and a broadcast — and keeps its
/// [`Notify`] private. Every signal in this workspace wraps the same `Notify`
/// type, so a public payload would let the config-change channel be fed
/// anywhere a `Notify` is wanted, including the constructors of signals that
/// carry a different authority (the connector reset, which belongs to a
/// system resume). Keeping the payload private makes that wiring fail to
/// compile instead of silently binding the wrong channel.
#[derive(Debug, Clone)]
pub struct ConfigChangeSignal(Notify);
impl ConfigChangeSignal {
    pub fn new() -> Self {
        Self(Notify::new())
    }

    /// A subscriber that observes every broadcast made after this call.
    pub fn subscription(&self) -> Subscription {
        self.0.subscription()
    }

    /// Broadcast a change to every current subscriber.
    pub fn notify_waiters(&self) {
        self.0.notify_waiters();
    }
}
impl Default for ConfigChangeSignal {
    fn default() -> Self {
        Self::new()
    }
}
