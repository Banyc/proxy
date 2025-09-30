use std::sync::{Arc, Mutex, atomic::AtomicUsize};

#[derive(Debug)]
struct WaiterHandler {
    pub trigger: tokio::sync::mpsc::Sender<()>,
    pub index: Arc<AtomicUsize>,
}
type Waiters = Vec<WaiterHandler>;

#[derive(Debug)]
pub struct Waiter {
    event: tokio::sync::mpsc::Receiver<()>,
    waiter_index: Arc<AtomicUsize>,
    waiters: Arc<Mutex<Waiters>>,
}
impl Waiter {
    pub fn has_notified(&self) -> bool {
        !self.event.is_empty()
    }
    pub async fn notified(&mut self) {
        self.event.recv().await.unwrap();
    }
}
impl Drop for Waiter {
    fn drop(&mut self) {
        let mut waiters = self.waiters.lock().unwrap();
        let index = self.waiter_index.load(std::sync::atomic::Ordering::Relaxed);
        waiters.swap_remove(index);
        let Some(replacer) = waiters.get(index) else {
            return;
        };
        replacer
            .index
            .store(index, std::sync::atomic::Ordering::Relaxed);
    }
}

#[derive(Debug, Clone)]
pub struct Notify {
    waiters: Arc<Mutex<Waiters>>,
}
impl Notify {
    pub fn new() -> Self {
        Self {
            waiters: Arc::new(Mutex::new(vec![])),
        }
    }
    pub fn waiter(&self) -> Waiter {
        let waiters_ref = self.waiters.clone();
        let mut waiters = self.waiters.lock().unwrap();
        let new_index = Arc::new(AtomicUsize::new(waiters.len()));
        let (tx, rx) = tokio::sync::mpsc::channel(1);
        let waiter_handler = WaiterHandler {
            trigger: tx,
            index: new_index.clone(),
        };
        waiters.push(waiter_handler);
        Waiter {
            event: rx,
            waiter_index: new_index.clone(),
            waiters: waiters_ref.clone(),
        }
    }
    pub fn notify_waiters(&self) {
        let mut waiters = self.waiters.lock().unwrap();
        for waiter in &mut *waiters {
            assert!(!waiter.trigger.is_closed());
            let _ = waiter.trigger.try_send(());
        }
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

        n.notify_waiters();
        w2.notified().await;
        w1.notified().await;

        drop(w1);
        assert_eq!(n.waiters.lock().unwrap().len(), 1);

        n.notify_waiters();
        w2.notified().await;

        drop(w2);
        assert_eq!(n.waiters.lock().unwrap().len(), 0);
    }
}
