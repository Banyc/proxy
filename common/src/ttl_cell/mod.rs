//! A time-expiring cell: holds an optional value that reads as `None` once a
//! fixed `lifetime` elapses since the last [`TtlCell::set`], with no background
//! timer. Used for cached route/selector state that should go stale on its own.

use std::{
    sync::Arc,
    time::{Duration, Instant},
};

use crate::clock::{Clock, SystemClock};

#[derive(Debug)]
pub struct TtlCell<T> {
    item: Option<T>,
    last_update: Instant,
    lifetime: Duration,
    clock: Arc<dyn Clock>,
}
impl<T> TtlCell<T> {
    pub fn new(item: Option<T>, lifetime: Duration) -> Self {
        Self::with_clock(item, lifetime, Arc::new(SystemClock))
    }

    /// Construct with an explicit clock. Production uses [`TtlCell::new`]
    /// (the system clock); the seam lets a test place `last_update` and the
    /// current instant exactly `lifetime` apart, so the exclusive expiry
    /// boundary is drivable without waiting real time.
    pub fn with_clock(item: Option<T>, lifetime: Duration, clock: Arc<dyn Clock>) -> Self {
        let last_update = clock.now();
        Self {
            item,
            last_update,
            lifetime,
            clock,
        }
    }

    pub fn get(&self) -> Option<&T> {
        if self.clock.now().saturating_duration_since(self.last_update) > self.lifetime {
            return None;
        }
        self.item.as_ref()
    }

    pub fn set(&mut self, item: T) -> &T {
        self.item = Some(item);
        self.last_update = self.clock.now();
        self.item.as_ref().unwrap()
    }

    /// Return the cached item while it is fresh; otherwise set it from `f`
    /// and return the freshly-set value directly (without re-checking the
    /// TTL, so sub-nanosecond lifetimes cannot expire it between set and
    /// read).
    pub fn get_or_set_with(&mut self, f: impl FnOnce() -> T) -> &T {
        if self.get().is_none() {
            self.item = Some(f());
            self.last_update = self.clock.now();
        }
        self.item.as_ref().unwrap()
    }
}

pub struct RegeneratingHeader {
    ttl: TtlCell<Arc<[u8]>>,
    regenerate: Box<dyn Fn() -> Arc<[u8]> + Send>,
}
impl RegeneratingHeader {
    pub fn new(regenerate: Box<dyn Fn() -> Arc<[u8]> + Send>, lifetime: Duration) -> Self {
        Self {
            ttl: TtlCell::new(None, lifetime),
            regenerate,
        }
    }

    pub fn get(&mut self) -> &Arc<[u8]> {
        self.ttl.get_or_set_with(|| (self.regenerate)())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[test]
    fn a_regenerating_header_caches_until_the_ttl_expires() {
        let calls = Arc::new(AtomicUsize::new(0));
        let regenerate = {
            let calls = Arc::clone(&calls);
            Box::new(move || {
                calls.fetch_add(1, Ordering::SeqCst);
                let n = calls.load(Ordering::SeqCst);
                Arc::from(vec![n as u8])
            })
        };
        let mut header = RegeneratingHeader::new(regenerate, Duration::from_secs(60));
        let first = header.get().clone();
        let cached = header.get().clone();
        assert_eq!(
            first, cached,
            "the value must be cached until the TTL expires"
        );
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn an_expired_header_regenerates() {
        let calls = Arc::new(AtomicUsize::new(0));
        let regenerate = {
            let calls = Arc::clone(&calls);
            Box::new(move || {
                calls.fetch_add(1, Ordering::SeqCst);
                let n = calls.load(Ordering::SeqCst);
                Arc::from(vec![n as u8])
            })
        };
        let mut header = RegeneratingHeader::new(regenerate, Duration::from_nanos(1));
        let _ = header.get();
        std::thread::sleep(Duration::from_micros(1));
        let _ = header.get();
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }

    /// The expiry guard is strictly exclusive: an item whose age is exactly
    /// `lifetime` is still fresh, and only a strictly larger age expires it.
    /// A `>`→`>=` mutation (or an inclusive rewrite) differs only at the
    /// exact instant, which no wall-clock test can reach; the injected clock
    /// places it deterministically.
    #[test]
    fn the_ttl_boundary_is_exclusive_at_exactly_the_lifetime() {
        let clock = Arc::new(crate::clock::test_support::ManualClock::new());
        let cell = TtlCell::with_clock(
            Some(7u8),
            Duration::from_secs(10),
            Arc::clone(&clock) as Arc<dyn Clock>,
        );
        clock.advance(Duration::from_secs(10));
        assert_eq!(
            cell.get(),
            Some(&7),
            "an item at exactly its lifetime must still be fresh"
        );
        clock.advance(Duration::from_nanos(1));
        assert_eq!(
            cell.get(),
            None,
            "an item one nanosecond past its lifetime must be expired"
        );
    }
}
