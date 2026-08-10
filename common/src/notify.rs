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
    /// Whether a notification has been broadcast since the last poll.
    ///
    /// A subscription created *after* a broadcast does not see it; each
    /// subscription observes only broadcasts that follow its creation (or its
    /// last consume).
    pub fn has_notified(&self) -> bool {
        // Errors only if the channel closed; `_tx` keeps it open for as long
        // as this subscription exists.
        self.rx.has_changed().unwrap_or(false)
    }
    /// Consume one pending notification, returning whether one was pending.
    pub fn remove_notified(&mut self) -> bool {
        let pending = self.has_notified();
        // Mark the current generation as seen so that only broadcasts made
        // after this point are reported by later polls.
        self.rx.mark_unchanged();
        pending
    }
    pub async fn notified(&mut self) {
        // unwrap: `self` holds a sender clone through `_tx`, so the channel
        // never closes while this subscription lives.
        self.rx.changed().await.unwrap();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn fresh_subscription_sees_no_pending_notification() {
        let n = Notify::new();
        let mut w1 = n.subscription();
        assert!(!w1.has_notified());

        n.notify_waiters();
        // A subscription created after a broadcast does not observe it.
        let w2 = n.subscription();
        assert!(!w2.has_notified());

        w1.notified().await;
        assert!(!w1.has_notified());
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
        assert!(!w1.has_notified());
        assert!(!w2.has_notified());
        assert!(!w3.has_notified());

        // A later broadcast wakes them again.
        n.notify_waiters();
        w1.notified().await;
        w2.notified().await;
        w3.notified().await;
        assert!(!w1.has_notified());
        assert!(!w2.has_notified());
        assert!(!w3.has_notified());
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
        assert!(!w.has_notified());
    }

    #[tokio::test]
    async fn remove_notified_consumes_a_pending_notification() {
        let n = Notify::new();
        let mut w = n.subscription();

        assert!(!w.remove_notified());
        n.notify_waiters();
        assert!(w.has_notified());
        assert!(w.remove_notified());
        assert!(!w.has_notified());
    }

    #[tokio::test]
    async fn dropping_a_subscriber_does_not_break_the_notifier() {
        let n = Notify::new();
        let mut live = n.subscription();
        drop(n.subscription());

        n.notify_waiters();
        assert!(live.has_notified());
        live.notified().await;
        assert!(!live.has_notified());
    }
}
