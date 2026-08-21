//! Task-scope epilogs for the proxy.
//!
//! Owner-requested cancellation is expected: `abort_all` + `join_next`
//! skipping cancelled joins is the normal shutdown shape.  But a child that
//! already produced a value, error, or panic before the owner aborted must
//! remain observable — `abort_and_reap` never swallows a completed panic as
//! cancellation, and `abort_and_reap_results` lets an owner-selected error
//! win over later ordinary child errors.

use tokio::task::JoinSet;

/// Abort every child of `tasks` and reap them, discarding completed values.
/// A completed (non-cancelled) join — including a panic — is unwrapped and
/// re-raised, so a child that beat the cancellation still surfaces.
pub async fn abort_and_reap<T: 'static>(tasks: &mut JoinSet<T>) {
    abort_and_reap_with(tasks, |_| {}).await;
}

/// Abort every child of `tasks`, reap them, and hand each completed value to
/// `observe`.  Cancelled joins are skipped (owner-requested cancellation is
/// expected); everything else — values, errors, panics — is observed.
pub async fn abort_and_reap_with<T: 'static>(tasks: &mut JoinSet<T>, mut observe: impl FnMut(T)) {
    tasks.abort_all();
    while let Some(joined) = tasks.join_next().await {
        if joined
            .as_ref()
            .is_err_and(tokio::task::JoinError::is_cancelled)
        {
            continue;
        }
        observe(joined.unwrap());
    }
}

/// Abort and reap a set of `Result`-producing children, folding their
/// outcomes into `outcome`.  The owner-selected `outcome` wins: a later
/// ordinary child error only replaces it while it is still `Ok`.
pub(crate) async fn abort_and_reap_results<E: 'static>(
    tasks: &mut JoinSet<Result<(), E>>,
    mut outcome: Result<(), E>,
) -> Result<(), E> {
    tasks.abort_all();
    while let Some(joined) = tasks.join_next().await {
        if joined
            .as_ref()
            .is_err_and(tokio::task::JoinError::is_cancelled)
        {
            continue;
        }
        let child_outcome = joined.unwrap();
        if outcome.is_ok() {
            outcome = child_outcome;
        }
    }
    outcome
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn cancelled_children_are_reaped() {
        let mut tasks: JoinSet<u32> = JoinSet::new();
        tasks.spawn(std::future::pending());
        abort_and_reap(&mut tasks).await;
        assert!(tasks.is_empty());
    }

    #[tokio::test]
    #[should_panic(expected = "child panic")]
    async fn generic_scope_cascades_a_completed_panic() {
        let mut tasks: JoinSet<u32> = JoinSet::new();
        let handle = tasks.spawn(async {
            panic!("child panic");
        });
        while !handle.is_finished() {
            tokio::task::yield_now().await;
        }
        abort_and_reap(&mut tasks).await;
    }

    #[tokio::test]
    async fn generic_scope_observes_a_completed_value() {
        let mut tasks: JoinSet<u32> = JoinSet::new();
        let handle = tasks.spawn(async { 7 });
        while !handle.is_finished() {
            tokio::task::yield_now().await;
        }
        let mut observed = Vec::new();
        abort_and_reap_with(&mut tasks, |value| observed.push(value)).await;
        assert_eq!(observed, vec![7]);
    }

    #[tokio::test]
    async fn completed_error_is_not_lost_to_cancellation() {
        let mut tasks: JoinSet<Result<(), &'static str>> = JoinSet::new();
        let handle = tasks.spawn(async { Err("child failed") });
        while !handle.is_finished() {
            tokio::task::yield_now().await;
        }
        let outcome = abort_and_reap_results(&mut tasks, Ok(())).await;
        assert_eq!(outcome, Err("child failed"));
    }

    #[tokio::test]
    async fn owner_error_precedes_a_later_child_error() {
        let mut tasks: JoinSet<Result<(), &'static str>> = JoinSet::new();
        let handle = tasks.spawn(async { Err("child error") });
        while !handle.is_finished() {
            tokio::task::yield_now().await;
        }
        // The owner already selected an error; a later child error must not
        // replace it.
        let outcome = abort_and_reap_results(&mut tasks, Err("owner error")).await;
        assert_eq!(outcome, Err("owner error"));
    }

    #[tokio::test]
    #[should_panic(expected = "racing panic")]
    async fn completed_panic_cascades() {
        let mut tasks: JoinSet<Result<(), &'static str>> = JoinSet::new();
        let handle = tasks.spawn(async {
            panic!("racing panic");
        });
        while !handle.is_finished() {
            tokio::task::yield_now().await;
        }
        let _ = abort_and_reap_results(&mut tasks, Ok(())).await;
    }
}
