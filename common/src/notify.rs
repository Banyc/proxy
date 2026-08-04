use std::sync::{Arc, Mutex};

use crate::notify::iter_set::{GuardedIterSet, IterSetEntryGuard};

fn binary_event_channel() -> (EdgeSignalTx, EdgeSignalRx) {
    let (tx, rx) = tokio::sync::mpsc::channel(1);
    (EdgeSignalTx(tx), EdgeSignalRx(rx))
}
#[derive(Debug)]
struct EdgeSignalTx(pub tokio::sync::mpsc::Sender<()>);
#[derive(Debug)]
struct EdgeSignalRx(pub tokio::sync::mpsc::Receiver<()>);

mod iter_set;

#[derive(Debug)]
pub struct Subscription {
    event: EdgeSignalRx,
    _parent_guard: IterSetEntryGuard<EdgeSignalTx>,
}
impl Subscription {
    pub fn has_notified(&self) -> bool {
        !self.event.0.is_empty()
    }
    pub fn remove_notified(&mut self) -> bool {
        self.event.0.try_recv().is_ok()
    }
    pub async fn notified(&mut self) {
        // unwrap: `self` still holds the event tx through `self._parent_guard`
        self.event.0.recv().await.unwrap();
    }
}

#[derive(Debug, Clone, Default)]
struct Subscribers {
    waiters: GuardedIterSet<EdgeSignalTx>,
    child_notifies: GuardedIterSet<Self>,
}
impl Subscribers {
    pub fn notify_waiters(&self) {
        self.waiters.values_mut(|waiter| {
            let _ = waiter.0.try_send(());
        });
        self.child_notifies.values_mut(|notify| {
            notify.notify_waiters();
        });
    }
    pub fn subscription(&self) -> Subscription {
        let (tx, rx) = binary_event_channel();
        let guard = self.waiters.add(tx);
        Subscription {
            event: rx,
            _parent_guard: guard,
        }
    }
    #[must_use]
    pub fn add_child_targets(&self, child: Self) -> IterSetEntryGuard<Self> {
        self.child_notifies.add(child)
    }
}

#[derive(Debug, Clone)]
pub struct Notify {
    parent_guard: Arc<Mutex<Vec<IterSetEntryGuard<Subscribers>>>>,
    targets: Subscribers,
}
impl Notify {
    pub fn new() -> Self {
        Self {
            targets: Subscribers::default(),
            parent_guard: Arc::new(Mutex::new(vec![])),
        }
    }
    pub fn strong_add_child_notify(&self, child: &Self) {
        let child_guard = self.targets.add_child_targets(child.targets.clone());
        child_guard.leak();
    }
    pub fn weak_add_child_notify(&self, child: &Self) {
        let child_guard = self.targets.add_child_targets(child.targets.clone());
        child.parent_guard.lock().unwrap().push(child_guard);
    }
    pub fn subscription(&self) -> Subscription {
        self.targets.subscription()
    }
    pub fn notify_waiters(&self) {
        self.targets.notify_waiters();
    }
}
impl Default for Notify {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_notified() {
        let n = Notify::new();
        let mut w1 = n.subscription();
        assert!(!w1.has_notified());

        n.notify_waiters();
        let mut w2 = n.subscription();
        assert!(!w2.has_notified());

        w1.notified().await;
        assert!(!w1.has_notified());

        let n2 = Notify::new();
        let n3 = Notify::new();
        n.weak_add_child_notify(&n2);
        n.strong_add_child_notify(&n3);
        assert_eq!(n.targets.child_notifies.len(), 2);

        let mut w3 = n2.subscription();
        assert!(!w3.has_notified());

        n.notify_waiters();
        w2.notified().await;
        w1.notified().await;
        w3.notified().await;
        assert!(!w1.has_notified());
        assert!(!w2.has_notified());
        assert!(!w3.has_notified());

        drop(w1);
        assert_eq!(n.targets.waiters.len(), 1);

        n.notify_waiters();
        w2.notified().await;
        w3.notified().await;
        assert!(!w2.has_notified());
        assert!(!w3.has_notified());

        drop(w2);
        assert_eq!(n.targets.waiters.len(), 0);

        drop(n3);
        assert_eq!(n.targets.child_notifies.len(), 2);

        drop(n2);
        assert_eq!(n.targets.child_notifies.len(), 1);
    }

    #[tokio::test]
    async fn a_half_dropped_waiter_does_not_panic_the_notifier() {
        let n = Notify::new();
        let live = n.subscription();
        let Subscription {
            event,
            _parent_guard,
        } = n.subscription();
        drop(event);
        assert_eq!(n.targets.waiters.len(), 2);
        n.notify_waiters();
        assert!(live.has_notified());
        drop(_parent_guard);
        assert_eq!(n.targets.waiters.len(), 1);
    }
}
