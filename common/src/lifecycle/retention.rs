use std::{
    any::Any,
    future::Future,
    pin::Pin,
    sync::Arc,
    time::{Duration, Instant},
};

use tokio::sync::mpsc;

use crate::{
    clock::{Clock, SystemClock},
    lifecycle::process::RootTaskExit,
};

/// Burst buffer for retained guards. Retention submissions happen once per
/// session/tunnel teardown, so steady-state occupancy is tiny; the actor
/// drains continuously. When full, `retain` backpressures the (async) caller,
/// which keeps the guard alive in the caller's scope until it can submit.
const RETENTION_CHANNEL_CAPACITY: usize = 256;

/// A guard submitted to the retention actor, kept alive until `until`.
struct Retain {
    guard: Box<dyn Any + Send>,
    until: Instant,
}

/// A cloneable handle to the process-lifetime retention actor.
#[derive(Clone)]
pub struct RetentionActorSender {
    tx: mpsc::Sender<Retain>,
}

impl RetentionActorSender {
    /// Keep `guard` alive at least until `until`, then drop it.
    pub async fn retain(&self, guard: Box<dyn Any + Send>, until: Instant) {
        let _ = self.tx.send(Retain { guard, until }).await;
    }
}

impl std::fmt::Debug for RetentionActorSender {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RetentionActorSender")
            .finish_non_exhaustive()
    }
}

/// Root-owned actor that keeps delayed-epilogue guards alive until their
/// deadline, then drops them. Supervised by the process root task scope.
pub struct RetentionActor {
    rx: mpsc::Receiver<Retain>,
    guards: Vec<(Instant, Box<dyn Any + Send>)>,
    clock: Arc<dyn Clock>,
}

impl RetentionActor {
    pub fn new() -> (Self, RetentionActorSender) {
        Self::with_clock(Arc::new(SystemClock))
    }

    /// Construct with an explicit clock. Production uses [`RetentionActor::new`]
    /// (the system clock); a test can advance the clock to a guard's deadline
    /// exactly, so the expiry boundary is deterministic.
    pub fn with_clock(clock: Arc<dyn Clock>) -> (Self, RetentionActorSender) {
        let (tx, rx) = mpsc::channel(RETENTION_CHANNEL_CAPACITY);
        (
            Self {
                rx,
                guards: Vec::new(),
                clock,
            },
            RetentionActorSender { tx },
        )
    }

    /// Run the retention loop until every sender is dropped.
    pub async fn run(mut self) -> RootTaskExit {
        loop {
            let sleep: Pin<Box<dyn Future<Output = ()> + Send>> = self
                .guards
                .iter()
                .map(|(until, _)| *until)
                .min()
                .map(|deadline| {
                    let now = self.clock.now();
                    let duration = if deadline > now {
                        deadline - now
                    } else {
                        Duration::ZERO
                    };
                    Box::pin(tokio::time::sleep(duration))
                        as Pin<Box<dyn Future<Output = ()> + Send>>
                })
                .unwrap_or_else(|| {
                    Box::pin(std::future::pending::<()>())
                        as Pin<Box<dyn Future<Output = ()> + Send>>
                });
            tokio::select! {
                msg = self.rx.recv() => {
                    match msg {
                        Some(retain) => self.guards.push((retain.until, retain.guard)),
                        None => return RootTaskExit::Completed { task: "retention_actor" },
                    }
                }
                () = sleep => {
                    let now = self.clock.now();
                    self.guards.retain(|(until, _)| *until > now);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    };

    struct DropGuard(Arc<AtomicBool>);
    impl Drop for DropGuard {
        fn drop(&mut self) {
            self.0.store(true, Ordering::SeqCst);
        }
    }

    #[tokio::test]
    async fn a_guard_is_dropped_after_its_deadline() {
        let dropped = Arc::new(AtomicBool::new(false));
        let (actor, sender) = RetentionActor::new();
        let mut tasks = tokio::task::JoinSet::new();
        tasks.spawn(actor.run());
        let guard = Box::new(DropGuard(dropped.clone()));
        sender
            .retain(guard, Instant::now() + Duration::from_millis(50))
            .await;
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert!(dropped.load(Ordering::SeqCst), "guard must be dropped");
        drop(sender);
        while let Some(result) = tasks.join_next().await {
            result.unwrap();
        }
    }

    #[tokio::test]
    async fn a_guard_is_kept_until_its_deadline() {
        let dropped = Arc::new(AtomicBool::new(false));
        let (actor, sender) = RetentionActor::new();
        let mut tasks = tokio::task::JoinSet::new();
        tasks.spawn(actor.run());
        let guard = Box::new(DropGuard(dropped.clone()));
        sender
            .retain(guard, Instant::now() + Duration::from_secs(60))
            .await;
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(
            !dropped.load(Ordering::SeqCst),
            "guard must outlive the deadline"
        );
        drop(sender);
        while let Some(result) = tasks.join_next().await {
            result.unwrap();
        }
    }

    /// The expiry guard is exclusive of the deadline: a guard whose `until`
    /// equals `now` is dropped, not kept. A `>`→`>=` mutation differs only at
    /// that exact instant; the injected clock places the actor's wakeup
    /// exactly on the deadline so the boundary is deterministic.
    #[tokio::test(start_paused = true)]
    async fn a_guard_is_dropped_at_exactly_its_deadline() {
        let clock = Arc::new(crate::clock::test_support::VirtualClock::new());
        let dropped = Arc::new(AtomicBool::new(false));
        let (actor, sender) = RetentionActor::with_clock(Arc::clone(&clock) as Arc<dyn Clock>);
        let mut tasks = tokio::task::JoinSet::new();
        tasks.spawn(actor.run());
        let guard = Box::new(DropGuard(dropped.clone()));
        let deadline = clock.now() + Duration::from_millis(100);
        sender.retain(guard, deadline).await;
        tokio::task::yield_now().await;
        tokio::time::advance(Duration::from_millis(100)).await;
        tokio::task::yield_now().await;
        tokio::task::yield_now().await;
        assert!(
            dropped.load(Ordering::SeqCst),
            "a guard at exactly its deadline is expired; the boundary is exclusive"
        );
        drop(sender);
        while let Some(result) = tasks.join_next().await {
            result.unwrap();
        }
    }
}
