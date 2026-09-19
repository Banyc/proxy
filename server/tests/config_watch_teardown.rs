//! Regression test for the `file_watcher_tokio` teardown abort.
//!
//! `file_watcher_tokio`'s FSEvents callback is
//! `tx.blocking_send(res).unwrap()` running inside a C callback. If the
//! `watch_file` future is dropped (which closes the receiver it owns) while a
//! delivery is in flight, the failed send panics inside that callback, which
//! cannot unwind, and aborts the whole process. `watch_file` owns both the
//! receiver and the `RecommendedWatcher`, so no caller can tear the watcher
//! down before the receiver; [`spawn_watch_tasks`] therefore keeps each
//! watcher's future on a detached OS thread whose receiver is still open when
//! the process exits.
//!
//! The teardown is exercised in a re-executed child so an abort surfaces as a
//! non-zero child exit instead of killing this test harness.

use std::{process::Command, sync::Arc, time::Duration};

use common::lifecycle::process::RootTaskExit;
use server::config::spawn_watch_tasks;

/// Set in the re-executed child to select the teardown body.
const CHILD_ENV: &str = "PROXY_CONFIG_WATCH_TEARDOWN_CHILD";
/// Enough watcher lifecycles that, without the fix, a late delivery lands on a
/// closed receiver at least once per child (measured 12/12 aborting).
const ATTEMPTS: usize = 30;

#[test]
fn tearing_down_the_config_watcher_does_not_abort() {
    if std::env::var_os(CHILD_ENV).is_some() {
        for _ in 0..ATTEMPTS {
            attempt_teardown();
        }
        return;
    }

    let exe = std::env::current_exe().unwrap();
    let output = Command::new(&exe)
        .env(CHILD_ENV, "1")
        .args([
            "--exact",
            "tearing_down_the_config_watcher_does_not_abort",
            "--nocapture",
        ])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "config watcher teardown aborted the child: status={:?}\nstdout:\n{}\nstderr:\n{}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
}

fn attempt_teardown() {
    let dir = unique_temp_dir();
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("config.toml");
    std::fs::write(&path, "initial = 1\n").unwrap();
    let watched: Arc<str> = Arc::from(path.to_str().unwrap());

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();

    runtime.block_on(async {
        let mut process_tasks: tokio::task::JoinSet<RootTaskExit> = tokio::task::JoinSet::new();
        let signal = spawn_watch_tasks(&mut process_tasks, std::slice::from_ref(&watched));
        let mut subscription = signal.0.subscription();

        // The OS watcher registers asynchronously; retry the write until the
        // signal fires. The signal is the success condition, the timeout only
        // bounds each attempt.
        for attempt in 0..50 {
            std::fs::write(&path, format!("change = {attempt}\n")).unwrap();
            if tokio::time::timeout(Duration::from_millis(20), subscription.notified())
                .await
                .is_ok()
            {
                break;
            }
        }
        // Saturate the watcher's bounded channel so a delivery is in flight at
        // teardown.
        for i in 0..200 {
            std::fs::write(&path, format!("flood = {i}\n")).unwrap();
        }
        // Mirror the process-root epilog: abort and reap the task set. A
        // watcher whose receiver lives in this set would close the channel
        // here while a late delivery is in flight.
        common::lifecycle::task_scope::abort_and_reap_with(&mut process_tasks, |_| {}).await;
    });
    drop(runtime);
    std::fs::remove_dir_all(&dir).ok();
}

fn unique_temp_dir() -> std::path::PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    std::env::temp_dir().join(format!(
        "proxy-watch-teardown-{}-{nanos}",
        std::process::id()
    ))
}
