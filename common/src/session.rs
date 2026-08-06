use std::{
    future::Future,
    pin::Pin,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
};
use tokio::sync::{mpsc, watch};

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
/// bumps a watch generation that wakes waiting submitters.
#[derive(Debug)]
struct Admission {
    limit: usize,
    live: AtomicUsize,
    capacity_changed: watch::Sender<u64>,
}
impl Admission {
    fn new(limit: usize) -> Self {
        assert!(limit > 0, "session admission limit must be non-zero");
        let (capacity_changed, _) = watch::channel(0_u64);
        Self {
            limit,
            live: AtomicUsize::new(0),
            capacity_changed,
        }
    }

    async fn acquire(self: &Arc<Self>) -> SessionPermit {
        let mut changed = self.capacity_changed.subscribe();
        loop {
            if let Some(permit) = self.try_acquire() {
                return permit;
            }
            changed
                .changed()
                .await
                .expect("Admission owns the watch sender while this Arc exists");
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
        let previous = self.admission.live.fetch_sub(1, Ordering::AcqRel);
        debug_assert!(previous > 0);
        self.admission.capacity_changed.send_modify(|generation| {
            *generation = generation.wrapping_add(1);
        });
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionSpawnError {
    AtCapacity,
    QueueFull,
    ScopeClosed,
}

impl SessionSpawnError {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::AtCapacity => "at_capacity",
            Self::QueueFull => "queue_full",
            Self::ScopeClosed => "scope_closed",
        }
    }
}

impl std::fmt::Display for SessionSpawnError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::error::Error for SessionSpawnError {}

pub fn log_rejection(protocol: &'static str, error: SessionSpawnError) {
    tracing::warn!(
        protocol,
        reason = error.as_str(),
        "rejecting new work at the process session scope"
    );
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
    pub fn channel() -> (Self, mpsc::Receiver<SessionFuture>) {
        Self::channel_with_limits(SESSION_SCOPE_CHANNEL_CAPACITY, SESSION_SCOPE_MAX_CONCURRENT)
    }

    /// [`Self::channel`] with a custom concurrent-session admission limit.
    pub(crate) fn channel_with_limit(limit: usize) -> (Self, mpsc::Receiver<SessionFuture>) {
        Self::channel_with_limits(SESSION_SCOPE_CHANNEL_CAPACITY, limit)
    }

    fn channel_with_limits(
        handoff_capacity: usize,
        limit: usize,
    ) -> (Self, mpsc::Receiver<SessionFuture>) {
        assert!(
            handoff_capacity > 0,
            "session handoff capacity must be non-zero"
        );
        let (tx, rx) = mpsc::channel(handoff_capacity);
        let admission = Arc::new(Admission::new(limit));
        (Self { tx, admission }, rx)
    }

    /// Submit a complete session future to the process session scope.
    ///
    /// Backpressures the caller while the scope's concurrent-session limit is
    /// reached (waiting for a slot to free) or the bounded channel is full.
    pub async fn spawn<F>(&self, fut: F) -> Result<(), SessionSpawnError>
    where
        F: Future<Output = SessionExit> + Send + 'static,
    {
        let permit = self.admission.acquire().await;
        let wrapped = async move {
            let _permit = permit;
            fut.await
        };
        self.tx
            .send(Box::pin(wrapped))
            .await
            .map_err(|_| SessionSpawnError::ScopeClosed)
    }

    /// Sync submission for handler closures that cannot await [`Self::spawn`]
    /// (e.g. the rtp_mux server's sync accept handler). Refuses with a typed
    /// reason when the scope's concurrency limit is reached, its bounded
    /// channel is full, or the scope is closed, in which case the caller must
    /// drop the session.
    pub fn try_spawn<F>(&self, fut: F) -> Result<(), SessionSpawnError>
    where
        F: Future<Output = SessionExit> + Send + 'static,
    {
        let permit = self
            .admission
            .try_acquire()
            .ok_or(SessionSpawnError::AtCapacity)?;
        let wrapped: SessionFuture = Box::pin(async move {
            let _permit = permit;
            fut.await
        });
        match self.tx.try_send(wrapped) {
            Ok(()) => Ok(()),
            Err(mpsc::error::TrySendError::Full(_)) => Err(SessionSpawnError::QueueFull),
            Err(mpsc::error::TrySendError::Closed(_)) => Err(SessionSpawnError::ScopeClosed),
        }
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
                .await
                .unwrap();
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
                .await
                .unwrap();
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
            .await
            .unwrap();
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
                .await
                .unwrap();
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
                .await
                .unwrap();
            let fut = tokio::time::timeout(Duration::from_secs(5), rx.recv())
                .await
                .expect("a session was never submitted")
                .unwrap();
            sessions.spawn(fut);
        }
        assert_eq!(
            spawner.try_spawn(async { Ok(()) }),
            Err(SessionSpawnError::AtCapacity),
            "try_spawn admitted a session past the concurrency limit"
        );
        // Freeing one slot lets the next submission through.
        release.notify_one();
        let _ = tokio::time::timeout(Duration::from_secs(5), sessions.join_next())
            .await
            .expect("the released session never completed")
            .unwrap()
            .unwrap();
        assert_eq!(
            spawner.try_spawn(async { Ok(()) }),
            Ok(()),
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
            .await
            .unwrap();
        let fut = tokio::time::timeout(Duration::from_secs(5), rx.recv())
            .await
            .expect("the first session was never submitted")
            .unwrap();
        sessions.spawn(fut);
        // The single slot is held; a second submission must wait.
        #[allow(clippy::disallowed_methods)]
        let waiting = tokio::task::spawn(async move {
            spawner.spawn(async { Ok(()) }).await.unwrap();
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

    #[tokio::test]
    async fn queued_sessions_consume_the_live_limit() {
        let (spawner, mut rx) = SessionSpawner::channel_with_limit(2);
        spawner.spawn(async { Ok(()) }).await.unwrap();
        spawner.spawn(async { Ok(()) }).await.unwrap();
        assert_eq!(
            spawner.try_spawn(async { Ok(()) }),
            Err(SessionSpawnError::AtCapacity),
            "a third submission must be refused while two sessions are queued"
        );
        assert_eq!(spawner.admission.live.load(Ordering::Acquire), 2);
        drop(rx.recv().await.unwrap());
        drop(rx.recv().await.unwrap());
        assert_eq!(spawner.admission.live.load(Ordering::Acquire), 0);
    }

    #[tokio::test]
    async fn queue_full_polls_back_the_reserved_permit() {
        let (spawner, mut rx) = SessionSpawner::channel_with_limits(1, 2);
        spawner.try_spawn(async { Ok(()) }).unwrap();
        assert_eq!(
            spawner.try_spawn(async { Ok(()) }),
            Err(SessionSpawnError::QueueFull),
            "a full handoff queue must refuse the submission"
        );
        assert_eq!(spawner.admission.live.load(Ordering::Acquire), 1);
        drop(rx.recv().await.unwrap());
        assert_eq!(
            spawner.try_spawn(async { Ok(()) }),
            Ok(()),
            "the reserved permit must be polled back after the queue drains"
        );
    }

    #[tokio::test]
    async fn closed_scope_is_reported_and_releases_permits() {
        let (spawner_a, rx_a) = SessionSpawner::channel_with_limit(2);
        let (spawner_b, rx_b) = SessionSpawner::channel_with_limit(2);
        spawner_a.try_spawn(async { Ok(()) }).unwrap();
        spawner_b.try_spawn(async { Ok(()) }).unwrap();
        drop(rx_a);
        drop(rx_b);
        assert_eq!(
            spawner_a.try_spawn(async { Ok(()) }),
            Err(SessionSpawnError::ScopeClosed),
            "a dropped scope must be reported as closed"
        );
        assert_eq!(
            spawner_b.try_spawn(async { Ok(()) }),
            Err(SessionSpawnError::ScopeClosed),
            "a dropped scope must be reported as closed"
        );
        assert_eq!(spawner_a.admission.live.load(Ordering::Acquire), 0);
        assert_eq!(spawner_b.admission.live.load(Ordering::Acquire), 0);
    }

    #[tokio::test]
    async fn watch_wakeup_releases_a_waiting_submitter() {
        let admission = Arc::new(Admission::new(1));
        let first = admission.try_acquire().unwrap();
        let waiting = admission.acquire();
        tokio::pin!(waiting);
        assert!(
            tokio::time::timeout(Duration::from_millis(20), &mut waiting,)
                .await
                .is_err()
        );
        drop(first);
        let second = tokio::time::timeout(Duration::from_secs(1), &mut waiting)
            .await
            .expect("capacity release did not wake the waiter");
        drop(second);
        assert_eq!(admission.live.load(Ordering::Acquire), 0);
    }
}
