//! Coalesced broadcast notification built on a [`tokio::sync::watch`]
//! generation counter.
//!
//! [`Notify::notify_waiters`] bumps a monotonically increasing `u64`
//! generation. Each [`Subscription`] holds a `watch` receiver, and a
//! subscriber is considered notified when the generation it last observed is
//! stale. Broadcasts are *coalesced*: any number of `notify_waiters` calls
//! between a subscriber's polls collapse into a single wakeup, because the
//! receiver only ever observes the latest generation, never intermediate
//! ones.
//!
//! The previous implementation was a hand-rolled registry (a mutex-guarded,
//! swap-remove-indexed entry set with a per-subscriber mpsc channel plus
//! parent/child fan-out). The parent/child machinery was unused outside its
//! own tests and has been removed along with the index-management code;
//! `watch` provides the same broadcast-with-coalescing guarantees natively.

use tokio::sync::watch;

/// A broadcast notification source. Cloneable; all clones share the same
/// generation counter.
#[derive(Debug, Clone)]
pub struct Notify {
    tx: watch::Sender<u64>,
}
impl Notify {
    pub fn new() -> Self {
        let (tx, _) = watch::channel(0);
        Self { tx }
    }
    /// Broadcast a wakeup to every current subscriber. Coalesced: a
    /// subscriber that polls later observes at most one pending
    /// notification no matter how many broadcasts happened in between.
    pub fn notify_waiters(&self) {
        self.tx.send_modify(|generation| *generation += 1);
    }
    pub fn subscription(&self) -> Subscription {
        Subscription {
            rx: self.tx.subscribe(),
            // Keep the channel alive for the subscription's lifetime, so
            // `changed()` can never observe the channel closing even if the
            // originating `Notify` is dropped.
            _tx: self.tx.clone(),
        }
    }
}
impl Default for Notify {
    fn default() -> Self {
        Self::new()
    }
}

/// A single subscriber to a [`Notify`].
#[derive(Debug)]
pub struct Subscription {
    rx: watch::Receiver<u64>,
    _tx: watch::Sender<u64>,
}
impl Subscription {
    pub async fn notified(&mut self) {
        // unwrap: `self` holds a sender clone through `_tx`, so the channel
        // never closes while this subscription lives.
        self.rx.changed().await.unwrap();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `true` if `notified()` would resolve immediately: a broadcast is
    /// pending since the last observed generation. Non-consuming — the
    /// pending notification stays pending for the caller to consume with
    /// `notified()` — implemented with `watch::Receiver::has_changed`
    /// rather than awaiting `notified()` under a timeout.
    fn pending(w: &mut Subscription) -> bool {
        w.rx.has_changed().unwrap_or(false)
    }

    #[tokio::test]
    async fn fresh_subscription_sees_no_pending_notification() {
        let n = Notify::new();
        let mut w1 = n.subscription();
        assert!(!pending(&mut w1));

        n.notify_waiters();
        // A subscription created after a broadcast does not observe it.
        let mut w2 = n.subscription();
        assert!(!pending(&mut w2));

        w1.notified().await;
        // Consumed: no second wakeup from the same broadcast.
        assert!(!pending(&mut w1));
    }

    #[tokio::test]
    async fn broadcast_reaches_every_subscriber() {
        let n = Notify::new();
        let mut w1 = n.subscription();
        let mut w2 = n.subscription();
        let mut w3 = n.subscription();

        n.notify_waiters();
        w1.notified().await;
        w2.notified().await;
        w3.notified().await;
        assert!(!pending(&mut w1));
        assert!(!pending(&mut w2));
        assert!(!pending(&mut w3));

        // A later broadcast wakes them again.
        n.notify_waiters();
        w1.notified().await;
        w2.notified().await;
        w3.notified().await;
        assert!(!pending(&mut w1));
        assert!(!pending(&mut w2));
        assert!(!pending(&mut w3));
    }

    #[tokio::test]
    async fn concurrent_broadcasts_coalesce_into_a_single_wakeup() {
        let n = Notify::new();
        let mut w = n.subscription();

        n.notify_waiters();
        n.notify_waiters();
        n.notify_waiters();
        // Three broadcasts between polls collapse into one wakeup.
        w.notified().await;
        assert!(!pending(&mut w));
    }

    #[tokio::test]
    async fn notified_consumes_a_pending_notification() {
        let n = Notify::new();
        let mut w = n.subscription();

        assert!(!pending(&mut w));
        n.notify_waiters();
        assert!(pending(&mut w));
        w.notified().await;
        assert!(!pending(&mut w));
    }

    #[tokio::test]
    async fn dropping_a_subscriber_does_not_break_the_notifier() {
        let n = Notify::new();
        let mut live = n.subscription();
        drop(n.subscription());

        n.notify_waiters();
        assert!(pending(&mut live));
        live.notified().await;
        assert!(!pending(&mut live));
    }
}
