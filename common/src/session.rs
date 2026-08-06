use std::{
    future::Future,
    pin::Pin,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
};
use tokio::sync::Notify;

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

/// Upper bound on live sessions admitted to the process session scope.
///
/// Admission is enforced atomically at submit time and released by an RAII
/// guard when the session completes, so the session `JoinSet` cannot grow
/// without bound even though `serve()` drains the submission channel into it
/// immediately. `spawn` backpressures while the scope is full;
/// `try_spawn` refuses.
pub const SESSION_SCOPE_MAX_CONCURRENT: usize = 4096;

/// Atomic admission for live sessions.
///
/// Acquiring a permit increments the live count; the RAII [`SessionPermit`]
/// releases the slot on drop (when the wrapped session future completes) and
/// wakes one waiting submitter via [`Notify`].
#[derive(Debug)]
struct Admission {
    limit: usize,
    live: AtomicUsize,
    notify: Notify,
}
impl Admission {
    fn new(limit: usize) -> Self {
        Self {
            limit,
            live: AtomicUsize::new(0),
            notify: Notify::new(),
        }
    }

    async fn acquire(self: &Arc<Self>) -> SessionPermit {
        loop {
            if let Some(permit) = self.try_acquire() {
                return permit;
            }
            self.notify.notified().await;
        }
    }

    fn try_acquire(self: &Arc<Self>) -> Option<SessionPermit> {
        let mut live = self.live.load(Ordering::Acquire);
        loop {
            if live >= self.limit {
                return None;
            }
            match self.live.compare_exchange_weak(
                live,
                live + 1,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => {
                    return Some(SessionPermit {
                        admission: Arc::clone(self),
                    });
                }
                Err(actual) => live = actual,
            }
        }
    }
}

/// RAII release guard for one admitted session slot. Dropped when the session
/// completes, freeing the slot and waking a waiting submitter.
#[derive(Debug)]
struct SessionPermit {
    admission: Arc<Admission>,
}
impl Drop for SessionPermit {
    fn drop(&mut self) {
        self.admission.live.fetch_sub(1, Ordering::AcqRel);
        self.admission.notify.notify_one();
    }
}

/// A cloneable handle to the process session scope.
///
/// `serve()` is the ONLY code that inserts into and reaps the underlying
/// `JoinSet`; everything else submits complete session futures through this
/// bounded sender. Submissions are admitted against an atomic concurrency
/// limit: `spawn` backpressures while the scope is full and `try_spawn`
/// refuses, so the session `JoinSet` stays bounded. If the scope is dropped,
/// submits are silently dropped (releasing their admission slots).
#[derive(Clone)]
pub struct SessionSpawner {
    tx: tokio::sync::mpsc::Sender<SessionFuture>,
    admission: Arc<Admission>,
}

impl SessionSpawner {
    /// Create a session scope sender and its receiver. The caller owns the
    /// receiver and the session `JoinSet`.
    pub fn channel() -> (Self, tokio::sync::mpsc::Receiver<SessionFuture>) {
        Self::channel_with_limit(SESSION_SCOPE_MAX_CONCURRENT)
    }

    /// [`Self::channel`] with a custom concurrent-session admission limit.
    pub(crate) fn channel_with_limit(
        limit: usize,
    ) -> (Self, tokio::sync::mpsc::Receiver<SessionFuture>) {
        let (tx, rx) = tokio::sync::mpsc::channel(SESSION_SCOPE_CHANNEL_CAPACITY);
        let admission = Arc::new(Admission::new(limit));
        (Self { tx, admission }, rx)
    }

    /// Submit a complete session future to the process session scope.
    ///
    /// Backpressures the caller while the scope's concurrent-session limit is
    /// reached (waiting for a slot to free) or the bounded channel is full.
    pub async fn spawn<F>(&self, fut: F)
    where
        F: Future<Output = SessionExit> + Send + 'static,
    {
        let permit = self.admission.acquire().await;
        let wrapped = async move {
            let _permit = permit;
            fut.await
        };
        let _ = self.tx.send(Box::pin(wrapped)).await;
    }

    /// Sync submission for handler closures that cannot await [`Self::spawn`]
    /// (e.g. the rtp_mux server's sync accept handler). Returns `false` if the
    /// scope's concurrency limit is reached or its bounded channel is full, in
    /// which case the caller must drop the session.
    pub fn try_spawn<F>(&self, fut: F) -> bool
    where
        F: Future<Output = SessionExit> + Send + 'static,
    {
        let Some(permit) = self.admission.try_acquire() else {
            return false;
        };
        let wrapped = async move {
            let _permit = permit;
            fut.await
        };
        self.tx.try_send(Box::pin(wrapped)).is_ok()
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
    use std::{
        sync::{
            Arc,
            atomic::{AtomicBool, AtomicUsize, Ordering},
        },
        time::Duration,
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
        let (finished_tx, finished_rx) = tokio::sync::oneshot::channel::<()>();
        {
            let listener_spawner = spawner.clone();
            listener_spawner
                .spawn(async move {
                    finished_rx.await.ok();
                    Ok(())
                })
                .await;
        }
        let fut = rx.recv().await.unwrap();
        sessions.spawn(fut);
        drop(spawner);
        drop(rx);
        finished_tx.send(()).ok();
        let res = sessions.join_next().await.unwrap();
        assert!(res.is_ok(), "the session survived listener removal");
    }

    #[tokio::test]
    async fn try_spawn_refuses_when_the_concurrency_limit_is_reached() {
        let (spawner, mut rx) = SessionSpawner::channel_with_limit(2);
        let mut sessions = tokio::task::JoinSet::new();
        let release = Arc::new(tokio::sync::Notify::new());
        for _ in 0..2 {
            let release = Arc::clone(&release);
            spawner
                .spawn(async move {
                    release.notified().await;
                    Ok(())
                })
                .await;
            let fut = tokio::time::timeout(Duration::from_secs(5), rx.recv())
                .await
                .expect("a session was never submitted")
                .unwrap();
            sessions.spawn(fut);
        }
        assert!(
            !spawner.try_spawn(async { Ok(()) }),
            "try_spawn admitted a session past the concurrency limit"
        );
        // Freeing one slot lets the next submission through.
        release.notify_one();
        let _ = tokio::time::timeout(Duration::from_secs(5), sessions.join_next())
            .await
            .expect("the released session never completed")
            .unwrap()
            .unwrap();
        assert!(
            spawner.try_spawn(async { Ok(()) }),
            "try_spawn stayed saturated after a session completed"
        );
        let fut = tokio::time::timeout(Duration::from_secs(5), rx.recv())
            .await
            .expect("the admitted session was never submitted")
            .unwrap();
        sessions.spawn(fut);
        // Free the other original slot, then drain everything.
        release.notify_one();
        while let Some(res) = tokio::time::timeout(Duration::from_secs(5), sessions.join_next())
            .await
            .expect("a session never completed")
        {
            res.unwrap().unwrap();
        }
    }

    #[tokio::test]
    async fn spawn_backpressures_until_a_slot_frees() {
        let (spawner, mut rx) = SessionSpawner::channel_with_limit(1);
        let mut sessions = tokio::task::JoinSet::new();
        let (release_tx, release_rx) = tokio::sync::oneshot::channel::<()>();
        spawner
            .spawn(async move {
                release_rx.await.ok();
                Ok(())
            })
            .await;
        let fut = tokio::time::timeout(Duration::from_secs(5), rx.recv())
            .await
            .expect("the first session was never submitted")
            .unwrap();
        sessions.spawn(fut);
        // The single slot is held; a second submission must wait.
        let waiting = tokio::task::spawn(async move {
            spawner.spawn(async { Ok(()) }).await;
            true
        });
        tokio::task::yield_now().await;
        assert!(
            !waiting.is_finished(),
            "spawn proceeded while the scope was at its concurrency limit"
        );
        release_tx.send(()).ok();
        let _ = tokio::time::timeout(Duration::from_secs(5), sessions.join_next())
            .await
            .expect("the released session never completed")
            .unwrap()
            .unwrap();
        let admitted = tokio::time::timeout(Duration::from_secs(5), waiting)
            .await
            .expect("the waiting submission never got a slot")
            .unwrap();
        assert!(admitted);
        // Drain the second session.
        let fut = tokio::time::timeout(Duration::from_secs(5), rx.recv())
            .await
            .expect("the second session was never submitted")
            .unwrap();
        sessions.spawn(fut);
        let _ = tokio::time::timeout(Duration::from_secs(5), sessions.join_next())
            .await
            .expect("the second session never completed")
            .unwrap()
            .unwrap();
    }
}
