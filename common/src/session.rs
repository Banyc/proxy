use std::{future::Future, pin::Pin};

use crate::error::AnyError;

/// Result of a completed session task.
pub type SessionExit = Result<(), AnyError>;

/// A boxed, complete session future.
pub type SessionFuture = Pin<Box<dyn Future<Output = SessionExit> + Send>>;

/// Burst buffer for session submissions.
///
/// `serve()` drains the channel every loop iteration and spawns each received
/// future into its `JoinSet` immediately, so in steady state the channel holds
/// at most a handful of submissions. The capacity only matters while a
/// hot-reload config reload is in flight (when `serve()` is busy loading and
/// not draining); once that is full, `spawn` backpressures the accept loop.
const SESSION_SCOPE_CHANNEL_CAPACITY: usize = 256;

/// A cloneable handle to the process session scope.
///
/// `serve()` is the ONLY code that inserts into and reaps the underlying
/// `JoinSet`; everything else submits complete session futures through this
/// bounded sender. If the scope is dropped, submits are silently dropped.
#[derive(Clone)]
pub struct SessionSpawner {
    tx: tokio::sync::mpsc::Sender<SessionFuture>,
}

impl SessionSpawner {
    /// Create a session scope sender and its receiver. The caller owns the
    /// receiver and the session `JoinSet`.
    pub fn channel() -> (Self, tokio::sync::mpsc::Receiver<SessionFuture>) {
        let (tx, rx) = tokio::sync::mpsc::channel(SESSION_SCOPE_CHANNEL_CAPACITY);
        (Self { tx }, rx)
    }

    /// Submit a complete session future to the process session scope.
    ///
    /// Backpressures the caller when the scope's bounded channel is full;
    /// `serve()` drains it continuously, so this only waits while the scope is
    /// saturated.
    pub async fn spawn<F>(&self, fut: F)
    where
        F: Future<Output = SessionExit> + Send + 'static,
    {
        let _ = self.tx.send(Box::pin(fut)).await;
    }

    /// Sync submission for handler closures that cannot await [`Self::spawn`]
    /// (e.g. the rtp_mux server's sync accept handler). Returns `false` if the
    /// scope's bounded channel is full or closed, in which case the caller
    /// must drop the session.
    pub fn try_spawn<F>(&self, fut: F) -> bool
    where
        F: Future<Output = SessionExit> + Send + 'static,
    {
        self.tx.try_send(Box::pin(fut)).is_ok()
    }
}

impl std::fmt::Debug for SessionSpawner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SessionSpawner").finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    };

    #[tokio::test]
    async fn a_scope_runs_complete_futures_and_normal_shutdown_drains_them() {
        let (spawner, mut rx) = SessionSpawner::channel();
        let mut sessions = tokio::task::JoinSet::new();
        let counter = Arc::new(AtomicUsize::new(0));
        for _ in 0..3 {
            let counter = counter.clone();
            spawner
                .spawn(async move {
                    counter.fetch_add(1, Ordering::SeqCst);
                    Ok(())
                })
                .await;
        }
        for _ in 0..3 {
            let fut = rx.recv().await.unwrap();
            sessions.spawn(fut);
        }
        while let Some(res) = sessions.join_next().await {
            res.unwrap().unwrap();
        }
        assert_eq!(counter.load(Ordering::SeqCst), 3);
    }

    #[tokio::test]
    async fn dropping_the_scope_aborts_all_remaining_tasks() {
        let (spawner, mut rx) = SessionSpawner::channel();
        let completed = Arc::new(AtomicBool::new(false));
        let started = Arc::new(tokio::sync::Notify::new());
        let mut sessions = tokio::task::JoinSet::new();
        for _ in 0..3 {
            spawner
                .spawn({
                    let completed = completed.clone();
                    let started = started.clone();
                    async move {
                        started.notified().await;
                        std::future::pending::<()>().await;
                        completed.store(true, Ordering::SeqCst);
                        Ok(())
                    }
                })
                .await;
            let fut = rx.recv().await.unwrap();
            sessions.spawn(fut);
        }
        started.notify_waiters();
        tokio::task::yield_now().await;
        drop(sessions);
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        assert!(
            !completed.load(Ordering::SeqCst),
            "scope drop must abort all remaining session tasks"
        );
    }

    #[tokio::test]
    async fn panics_are_observed_as_join_errors() {
        let (spawner, mut rx) = SessionSpawner::channel();
        let mut sessions = tokio::task::JoinSet::new();
        spawner
            .spawn(async {
                panic!("session panic");
            })
            .await;
        let fut = rx.recv().await.unwrap();
        sessions.spawn(fut);
        let res = sessions.join_next().await.unwrap();
        assert!(
            matches!(&res, Err(error) if error.is_panic()),
            "a panicking session must surface as a join error"
        );
    }

    #[tokio::test]
    async fn submitted_sessions_outlive_their_senders() {
        // Simulates listener hot reload: dropping a listener's spawner clone
        // (and even every sender) must not stop already-submitted sessions.
        let (spawner, mut rx) = SessionSpawner::channel();
        let mut sessions = tokio::task::JoinSet::new();
        let finished = Arc::new(tokio::sync::Notify::new());
        {
            let listener_spawner = spawner.clone();
            listener_spawner
                .spawn(async move {
                    finished.notified().await;
                    Ok(())
                })
                .await;
        }
        let fut = rx.recv().await.unwrap();
        sessions.spawn(fut);
        drop(spawner);
        drop(rx);
        finished.notify_waiters();
        let res = sessions.join_next().await.unwrap();
        assert!(res.is_ok(), "the session survived listener removal");
    }
}
