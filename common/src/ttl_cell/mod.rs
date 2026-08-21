//! A time-expiring cell: holds an optional value that reads as `None` once a
//! fixed `lifetime` elapses since the last [`TtlCell::set`], with no background
//! timer. Used for cached route/selector state that should go stale on its own.

use std::{
    sync::Arc,
    time::{Duration, Instant},
};

#[derive(Debug)]
pub struct TtlCell<T> {
    item: Option<T>,
    last_update: Instant,
    lifetime: Duration,
}
impl<T> TtlCell<T> {
    pub fn new(item: Option<T>, lifetime: Duration) -> Self {
        Self {
            item,
            last_update: Instant::now(),
            lifetime,
        }
    }

    pub fn get(&self) -> Option<&T> {
        if self.last_update.elapsed() > self.lifetime {
            return None;
        }
        self.item.as_ref()
    }

    pub fn set(&mut self, item: T) -> &T {
        self.item = Some(item);
        self.last_update = Instant::now();
        self.item.as_ref().unwrap()
    }

    /// Return the cached item while it is fresh; otherwise set it from `f`
    /// and return the freshly-set value directly (without re-checking the
    /// TTL, so sub-nanosecond lifetimes cannot expire it between set and
    /// read).
    pub fn get_or_set_with(&mut self, f: impl FnOnce() -> T) -> &T {
        if self.get().is_none() {
            self.item = Some(f());
            self.last_update = Instant::now();
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
}
