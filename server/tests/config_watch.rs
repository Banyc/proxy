//! Exercise the real on-disk config watcher: a write to a watched file must
//! signal the production [`ConfigChangeSignal`] the serve loop reloads on.
//!
//! [`spawn_watch_tasks`] runs each `file_watcher_tokio` watcher on a detached
//! OS thread whose receiver is never closed before process exit; that is what
//! makes the normal teardown below safe. The abort this avoids (a
//! `blocking_send(...).unwrap()` panicking inside the FSEvents C callback) is
//! pinned separately by `config_watch_teardown.rs`, which observes it as a
//! child-process abort.

use std::{sync::Arc, time::Duration};

use common::lifecycle::process::RootTaskExit;
use server::config::spawn_watch_tasks;

fn unique_temp_dir(tag: &str) -> std::path::PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    std::env::temp_dir().join(format!(
        "proxy-watch-test-{tag}-{}-{nanos}",
        std::process::id()
    ))
}

#[test]
fn a_real_file_change_signals_the_config_watcher() {
    let dir = unique_temp_dir("watch");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("config.toml");
    std::fs::write(&path, "initial = 1\n").unwrap();
    let watched: Arc<str> = Arc::from(path.to_str().unwrap());

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();

    let notified = runtime.block_on(async {
        let mut process_tasks: tokio::task::JoinSet<RootTaskExit> = tokio::task::JoinSet::new();
        let signal = spawn_watch_tasks(&mut process_tasks, std::slice::from_ref(&watched));
        let mut subscription = signal.subscription();

        // The OS watcher registers asynchronously, so retry the write until
        // the signal fires. The signal is the success condition; the timeout
        // only bounds each attempt.
        let mut notified = false;
        for attempt in 0..20 {
            std::fs::write(&path, format!("change = {attempt}\n")).unwrap();
            if tokio::time::timeout(Duration::from_millis(500), subscription.notified())
                .await
                .is_ok()
            {
                notified = true;
                break;
            }
        }
        // The watcher tasks may now be torn down normally: the watcher future
        // (and its receiver) lives on a detached thread, not in this set.
        notified
    });
    drop(runtime);

    std::fs::remove_dir_all(&dir).ok();
    assert!(
        notified,
        "a real change to the watched file must signal the config watcher"
    );
}
