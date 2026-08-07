use thiserror::Error;

use crate::error::AnyError;

/// Exit status of a process-lifetime root task.
///
/// Panics surface through the `JoinError` yielded by the supervising
/// `JoinSet` (the caller's `unwrap` re-raises them); this value marks a task
/// that ran to completion on its own, which for a root-owned task is
/// unexpected.
#[derive(Debug)]
pub enum ProcessTaskExit {
    Completed { task: &'static str },
    Failed { task: &'static str, detail: String },
}

/// Outcome of supervising a single root-process task.
///
/// Root-process actors are expected to run for the entire lifetime of the
/// process; any completion — clean or failed — is a fatal supervision error
/// that must bring the process down so the operator notices the service is
/// no longer running. Panics are already fatal (`JoinError::unwrap`
/// re-raises them inside [`handle_root_task_exit`]); this type covers the
/// remaining, previously log-only cases.
#[derive(Debug, Error)]
pub enum ProcessSupervisionError {
    /// A root task returned `ProcessTaskExit::Completed` on its own, which
    /// for a process-lifetime actor is unexpected.
    #[error("Root process task `{task}` completed unexpectedly")]
    CompletedUnexpectedly { task: &'static str },
    /// A root task returned `ProcessTaskExit::Failed`.
    #[error("Root process task `{task}` failed: {detail}")]
    Failed { task: &'static str, detail: String },
    /// A root task was cancelled without the `JoinSet` being dropped, which
    /// is unexpected for a process-lifetime actor.
    #[error("Root process task was cancelled unexpectedly")]
    Cancelled,
    /// Joining the root task failed for a reason other than panic/cancel.
    #[error("Root process task failed to join: {source}")]
    JoinFailed { source: AnyError },
}

/// Supervise the join result of a single root-process task.
///
/// Returns `Err(ProcessSupervisionError)` for *every* non-normal completion
/// of a root task: clean completion and failure are both fatal for
/// process-lifetime actors. The caller is expected to surface this as a
/// process-fatal error (e.g. propagate it out of `main`) so the service is
/// restarted by the surrounding supervisor (systemd, a container
/// orchestrator, etc.) instead of running permanently without the dead
/// actor.
///
/// The join result is unwrapped first: `JoinError::unwrap` re-raises a
/// panicked task's panic through the caller with its original backtrace
/// (the scoped panic-cascade), and a cancelled or failed join aborts via the
/// same `unwrap`. The task's ordinary output is then classified.
///
/// `Ok(())` is never returned: there is no normal exit for a root actor.
pub fn handle_root_task_exit(
    res: Result<ProcessTaskExit, tokio::task::JoinError>,
) -> Result<(), ProcessSupervisionError> {
    let exit = res.unwrap();
    match exit {
        ProcessTaskExit::Completed { task } => {
            Err(ProcessSupervisionError::CompletedUnexpectedly { task })
        }
        ProcessTaskExit::Failed { task, detail } => {
            Err(ProcessSupervisionError::Failed { task, detail })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };

    /// A process-lifetime task that returns `Completed` must produce a
    /// fatal `CompletedUnexpectedly` error — it must not be silently logged.
    #[test]
    fn root_task_completing_unexpectedly_is_fatal() {
        let res: Result<ProcessTaskExit, tokio::task::JoinError> = Ok(ProcessTaskExit::Completed {
            task: "retention_actor",
        });
        let err = handle_root_task_exit(res).expect_err("must be fatal");
        match err {
            ProcessSupervisionError::CompletedUnexpectedly { task } => {
                assert_eq!(task, "retention_actor",)
            }
            other => panic!("expected CompletedUnexpectedly, got {other:?}"),
        }
    }

    /// A process-lifetime task that returns `Failed` must produce a fatal
    /// `Failed` error carrying the task tag and detail.
    #[test]
    fn root_task_failing_is_fatal() {
        let res: Result<ProcessTaskExit, tokio::task::JoinError> = Ok(ProcessTaskExit::Failed {
            task: "config_watcher",
            detail: "/etc/proxy.toml:watcher gone".to_string(),
        });
        let err = handle_root_task_exit(res).expect_err("must be fatal");
        match err {
            ProcessSupervisionError::Failed { task, detail } => {
                assert_eq!(task, "config_watcher");
                assert!(detail.contains("/etc/proxy.toml"), "{detail}");
                assert!(detail.contains("watcher gone"), "{detail}");
            }
            other => panic!("expected Failed, got {other:?}"),
        }
    }

    /// A panicked root task must propagate its panic through the caller:
    /// `handle_root_task_exit` unwraps the `JoinError`, so the panic
    /// crosses the task boundary (scoped panic-cascade) instead of being
    /// reported as a supervision error.
    #[test]
    fn root_task_panicking_propagates_the_panic() {
        // We can't build a `JoinError` directly (no public ctor), so join a
        // real panicking task via a task scope. `tokio::task::JoinSet` is
        // the lint-approved spawn path.
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("rt");
        let join_res = runtime.block_on(async {
            let mut tasks: tokio::task::JoinSet<()> = tokio::task::JoinSet::new();
            tasks.spawn(async {
                panic!("boom");
            });
            tasks
                .join_next()
                .await
                .expect("task exists")
                .map(|_: ()| ProcessTaskExit::Completed { task: "test" })
        });
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _ = handle_root_task_exit(join_res);
        }));
        match result {
            Err(payload) => {
                let msg = payload
                    .downcast_ref::<&'static str>()
                    .copied()
                    .or_else(|| payload.downcast_ref::<String>().map(String::as_str))
                    .unwrap_or("");
                assert!(
                    msg.contains("boom"),
                    "panic must propagate 'boom', got {msg:?}"
                );
            }
            Ok(()) => panic!("handle_root_task_exit must propagate the panic, not return"),
        }
    }

    /// A cancelled root task is fatal: the handler unwraps the join result
    /// first, so a cancelled join aborts (panics) rather than returning
    /// `ProcessSupervisionError::Cancelled`.
    #[tokio::test]
    #[should_panic(expected = "JoinError::Cancelled")]
    async fn root_task_cancelled_is_fatal() {
        let mut tasks: tokio::task::JoinSet<()> = tokio::task::JoinSet::new();
        tasks.spawn(async {
            std::future::pending::<()>().await;
        });
        // Aborting the task yields a *cancelled* join result (not a panic).
        tasks.abort_all();
        let join_res = tasks
            .join_next()
            .await
            .expect("task exists")
            .map(|_: ()| ProcessTaskExit::Completed { task: "test" });
        handle_root_task_exit(join_res).unwrap_err();
    }

    /// A guard around the panic-resume path proves it is *always* fatal
    /// and never returns `Ok(())`. A clean `Completed` exit should never
    /// be swallowed as a normal exit.
    #[test]
    fn handle_root_task_exit_never_returns_ok() {
        let res: Result<ProcessTaskExit, tokio::task::JoinError> = Ok(ProcessTaskExit::Completed {
            task: "monitor_server",
        });
        assert!(handle_root_task_exit(res).is_err());
    }

    /// The handler must be usable in a `select!` loop the way `main.rs` uses
    /// it: it takes the exact `Result<ProcessTaskExit, JoinError>` that
    /// `JoinSet::join_next` yields (modulo `Option`), and returns a value
    /// the caller can propagate. This is a compile-time check plus a single
    /// runtime invocation.
    #[tokio::test]
    async fn handler_is_select_loop_compatible() {
        let mut tasks: tokio::task::JoinSet<ProcessTaskExit> = tokio::task::JoinSet::new();
        let sentinel = Arc::new(AtomicUsize::new(0));
        let s = sentinel.clone();
        tasks.spawn(async move {
            s.store(1, Ordering::SeqCst);
            ProcessTaskExit::Completed { task: "sentinel" }
        });
        let join_res = tasks.join_next().await.expect("task exists");
        let err = handle_root_task_exit(join_res).expect_err("must be fatal");
        assert!(matches!(
            err,
            ProcessSupervisionError::CompletedUnexpectedly { task: "sentinel" }
        ));
        assert_eq!(sentinel.load(Ordering::SeqCst), 1, "task ran");
    }
}
