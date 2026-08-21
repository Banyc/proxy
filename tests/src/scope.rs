use std::future::Future;

use common::{
    error::AnyResult,
    lifecycle::retention::{RetentionActor, RetentionActorSender},
    session::SessionSpawner,
};

/// How a scope-owned background task ended, surfaced by
/// [`TestRuntimeScope::run`].
#[derive(Debug)]
enum TaskEnd {
    /// A required task ended — cleanly or with an error. It should not have
    /// ended before the test body did, so either is a test failure.
    Required(AnyResult),
    /// A session task ended normally; expected and ignored.
    Session,
}

/// A scoped test runtime: owns every background task of a test — the
/// session actor, the retention actor, connector-driver reapers, proxy and
/// greet servers, and anything else the test spawns — in a single `JoinSet`
/// that the test body races against.
///
/// - [`Self::spawn_required`] adds a task whose ending (clean or failed)
///   fails the test through [`Self::run`].
/// - [`Self::spawn_session`] adds a task whose normal ending is expected;
///   only a panic fails the test.
/// - [`Self::run`] runs the test body against `join_next()`, unwrapping
///   both `JoinError`s and required-task results, and returns when the body
///   finishes. The remaining tasks are aborted when the scope drops.
pub struct TestRuntimeScope {
    tasks: tokio::task::JoinSet<TaskEnd>,
    session_spawner: SessionSpawner,
    retention: RetentionActorSender,
}

impl TestRuntimeScope {
    /// Spawn the process actors every test needs: the session actor (which
    /// services `SessionSpawner` submissions) and the retention actor (which
    /// keeps delayed-epilogue guards alive).
    pub fn new() -> Self {
        let mut tasks = tokio::task::JoinSet::new();
        let (session_spawner, mut session_rx) = SessionSpawner::channel();
        tasks.spawn(async move {
            let mut sessions = tokio::task::JoinSet::new();
            loop {
                tokio::select! {
                    Some(fut) = session_rx.recv() => {
                        sessions.spawn(fut);
                    }
                    Some(result) = sessions.join_next() => {
                        // A panicked session must fail the test; its result
                        // is the session's own business.
                        let _ = result.expect("session task panicked");
                    }
                    else => break,
                }
            }
            TaskEnd::Required(Ok(()))
        });
        let (retention_actor, retention) = RetentionActor::new();
        tasks.spawn(async move {
            retention_actor.run().await;
            TaskEnd::Required(Ok(()))
        });
        Self {
            tasks,
            session_spawner,
            retention,
        }
    }

    /// The session spawner backed by this scope's session actor.
    pub fn session_spawner(&self) -> SessionSpawner {
        self.session_spawner.clone()
    }

    /// The retention sender backed by this scope's retention actor.
    pub fn retention(&self) -> RetentionActorSender {
        self.retention.clone()
    }

    /// Spawn a required background task. Its `Err` result or a panic fails
    /// the test through [`Self::run`]; so does a clean exit before the test
    /// body finishes. Actors and proxy servers are required.
    pub fn spawn_required(&mut self, future: impl Future<Output = AnyResult> + Send + 'static) {
        self.tasks
            .spawn(async move { TaskEnd::Required(future.await) });
    }

    /// Spawn a session task whose normal completion is expected (greet
    /// servers, echo servers, parallel clients); a panic still fails the
    /// test through [`Self::run`].
    pub fn spawn_session(&mut self, future: impl Future<Output = ()> + Send + 'static) {
        self.tasks.spawn(async move {
            future.await;
            TaskEnd::Session
        });
    }

    /// Run the test body against the scope's background tasks: whichever
    /// ends first wins. If a background task ends first — a required task
    /// failed or exited, or any task panicked — the test fails. If the body
    /// finishes first, its output is returned and the remaining tasks are
    /// aborted when the scope drops.
    pub async fn run<T>(&mut self, body: impl Future<Output = T>) -> T {
        tokio::pin!(body);
        loop {
            tokio::select! {
                // `biased` with `join_next` first: a background task that
                // ended before the body (a failure or a panic) always wins
                // over an already-ready body, instead of being dropped by an
                // unbiased coin flip.
                biased;
                result = self.tasks.join_next() => {
                    let end = result
                        .expect("scope task set closed")
                        .expect("background task panicked");
                    match end {
                        TaskEnd::Required(Err(error)) => {
                            panic!("required background task failed: {error}");
                        }
                        TaskEnd::Required(Ok(())) => {
                            panic!("required background task exited before the test body");
                        }
                        // A session task ended as expected; keep running.
                        TaskEnd::Session => continue,
                    }
                }
                output = &mut body => return output,
            }
        }
    }
}

impl Default for TestRuntimeScope {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for TestRuntimeScope {
    fn drop(&mut self) {
        // The test body already finished; abort whatever is still running.
        self.tasks.abort_all();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[tokio::test]
    async fn run_returns_the_body_output_when_the_body_finishes_first() {
        let mut scope = TestRuntimeScope::new();
        scope.spawn_session(async {
            // A normally-completing session task must not disturb the run.
            std::future::pending::<()>().await;
        });
        let output = scope.run(async { 42 }).await;
        assert_eq!(output, 42);
    }

    #[tokio::test]
    #[should_panic(expected = "required background task failed: boom")]
    async fn a_failing_required_task_fails_the_run() {
        let mut scope = TestRuntimeScope::new();
        scope.spawn_required(async { Err("boom".into()) });
        // Pend so the required task's ending wins the race against the body.
        scope
            .run(async { std::future::pending::<()>().await })
            .await;
    }

    #[tokio::test]
    #[should_panic(expected = "required background task exited before the test body")]
    async fn a_cleanly_exiting_required_task_fails_the_run() {
        let mut scope = TestRuntimeScope::new();
        scope.spawn_required(async { Ok(()) });
        scope
            .run(async { std::future::pending::<()>().await })
            .await;
    }

    #[tokio::test]
    #[should_panic(expected = "background task panicked")]
    async fn a_panicking_session_task_fails_the_run() {
        let mut scope = TestRuntimeScope::new();
        scope.spawn_session(async {
            panic!("boom");
        });
        scope
            .run(async { std::future::pending::<()>().await })
            .await;
    }

    #[tokio::test]
    #[should_panic(expected = "background task panicked")]
    async fn a_ready_background_panic_wins_over_a_ready_body() {
        let mut scope = TestRuntimeScope::new();
        // The spawned task notifies and then panics in the same poll; the
        // body becomes ready only once that notification arrives, so both
        // `join_next()` (with the JoinError) and the body are ready at the
        // same moment when `run` selects. The biased select must surface the
        // panic instead of returning the body's output.
        let released = Arc::new(tokio::sync::Notify::new());
        scope.spawn_session({
            let released = Arc::clone(&released);
            async move {
                released.notify_one();
                panic!("boom");
            }
        });
        let _ = scope
            .run(async {
                released.notified().await;
                42
            })
            .await;
    }
}
