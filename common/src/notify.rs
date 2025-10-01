use std::sync::{Arc, Mutex};

use crate::notify::iter_set::{GuardedIterSet, IterSetEntryGuard};

fn binary_event_channel() -> (BinaryEventTx, BinaryEventRx) {
    let (tx, rx) = tokio::sync::mpsc::channel(1);
    (BinaryEventTx(tx), BinaryEventRx(rx))
}
#[derive(Debug)]
struct BinaryEventTx(pub tokio::sync::mpsc::Sender<()>);
#[derive(Debug)]
struct BinaryEventRx(pub tokio::sync::mpsc::Receiver<()>);

mod iter_set {
    use std::sync::{Arc, Mutex, atomic::AtomicUsize};

    use derive_more::Debug;

    #[derive(Debug)]
    pub struct IterSet<T> {
        buf: Vec<Entry<T>>,
    }
    #[derive(Debug)]
    struct Entry<T> {
        pub value: T,
        pub index: Arc<AtomicUsize>,
    }
    impl<T> Default for IterSet<T> {
        fn default() -> Self {
            Self { buf: vec![] }
        }
    }
    impl<T> IterSet<T> {
        pub fn values_mut(&mut self) -> impl Iterator<Item = &mut T> {
            self.buf.iter_mut().map(|entry| &mut entry.value)
        }
        pub fn append(&mut self, v: T) -> Arc<AtomicUsize> {
            let new_index = Arc::new(AtomicUsize::new(self.buf.len()));
            let waiter_handler = Entry {
                value: v,
                index: new_index.clone(),
            };
            self.buf.push(waiter_handler);
            new_index
        }
        pub fn remove(&mut self, i: usize) {
            self.buf.swap_remove(i);
            let Some(replacer) = self.buf.get(i) else {
                return;
            };
            replacer
                .index
                .store(i, std::sync::atomic::Ordering::Relaxed);
        }
        pub fn len(&self) -> usize {
            self.buf.len()
        }
    }
    #[derive(Debug)]
    #[debug(bound(T:))]
    pub struct GuardedIterSet<T> {
        ptr: Arc<Mutex<IterSet<T>>>,
    }
    impl<T> Clone for GuardedIterSet<T> {
        fn clone(&self) -> Self {
            Self {
                ptr: self.ptr.clone(),
            }
        }
    }
    impl<T> Default for GuardedIterSet<T> {
        fn default() -> Self {
            let buf = IterSet::default();
            let ptr = Arc::new(Mutex::new(buf));
            Self { ptr }
        }
    }
    #[allow(unused)]
    impl<T> GuardedIterSet<T> {
        #[must_use]
        pub fn add(&self, v: T) -> IterSetEntryGuard<T> {
            let ptr = self.ptr.clone();
            let mut buf = self.ptr.lock().unwrap();
            let index = buf.append(v);
            IterSetEntryGuard {
                buf: ptr,
                index,
                leak: false,
            }
        }
        pub fn values_mut(&self, mut f: impl FnMut(&mut T)) {
            let mut buf = self.ptr.lock().unwrap();
            for v in buf.values_mut() {
                f(v);
            }
        }
        pub fn len(&self) -> usize {
            self.ptr.lock().unwrap().len()
        }
    }
    #[derive(Debug)]
    pub struct IterSetEntryGuard<T> {
        buf: Arc<Mutex<IterSet<T>>>,
        index: Arc<AtomicUsize>,
        leak: bool,
    }
    impl<T> IterSetEntryGuard<T> {
        pub fn leak(mut self) {
            self.leak = true;
        }
    }
    impl<T> Drop for IterSetEntryGuard<T> {
        fn drop(&mut self) {
            if self.leak {
                return;
            }
            let mut buf = self.buf.lock().unwrap();
            let i = self.index.load(std::sync::atomic::Ordering::Relaxed);
            buf.remove(i);
        }
    }
}

#[derive(Debug)]
pub struct Waiter {
    event: BinaryEventRx,
    _parent_guard: IterSetEntryGuard<BinaryEventTx>,
}
impl Waiter {
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
struct NotifyTargets {
    waiters: GuardedIterSet<BinaryEventTx>,
    child_notifies: GuardedIterSet<Self>,
}
impl NotifyTargets {
    pub fn notify_waiters(&self) {
        self.waiters.values_mut(|waiter| {
            assert!(!waiter.0.is_closed());
            let _ = waiter.0.try_send(());
        });
        self.child_notifies
            .values_mut(|notify| notify.notify_waiters());
    }
    pub fn waiter(&self) -> Waiter {
        let (tx, rx) = binary_event_channel();
        let guard = self.waiters.add(tx);
        Waiter {
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
    parent_guard: Arc<Mutex<Vec<IterSetEntryGuard<NotifyTargets>>>>,
    targets: NotifyTargets,
}
impl Notify {
    pub fn new() -> Self {
        Self {
            targets: NotifyTargets::default(),
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
    pub fn waiter(&self) -> Waiter {
        self.targets.waiter()
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
        let mut w1 = n.waiter();

        n.notify_waiters();
        let mut w2 = n.waiter();
        w1.notified().await;

        let n2 = Notify::new();
        let n3 = Notify::new();
        n.weak_add_child_notify(&n2);
        n.strong_add_child_notify(&n3);
        assert_eq!(n.targets.child_notifies.len(), 2);

        let mut w3 = n2.waiter();
        n.notify_waiters();
        w2.notified().await;
        w1.notified().await;
        w3.notified().await;

        drop(w1);
        assert_eq!(n.targets.waiters.len(), 1);

        n.notify_waiters();
        w2.notified().await;
        w3.notified().await;

        drop(w2);
        assert_eq!(n.targets.waiters.len(), 0);

        drop(n3);
        assert_eq!(n.targets.child_notifies.len(), 2);

        drop(n2);
        assert_eq!(n.targets.child_notifies.len(), 1);
    }
}
