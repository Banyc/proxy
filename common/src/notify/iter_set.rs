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
// `len` is only read by the notify tests; keep `allow` because `expect`
// would go unfulfilled under `--all-targets`/test builds where it is used.
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::Ordering;

    /// Removing an entry swap-removes the last element into the vacated slot;
    /// that replacer's `index` must be patched to the vacated slot, or a later
    /// drop would remove the wrong entry.
    #[test]
    fn swap_remove_patches_the_replacer_s_index() {
        let mut set = IterSet::default();
        let a = set.append("a");
        let b = set.append("b");
        let c = set.append("c");
        let d = set.append("d");

        // Removing index 1 swap-removes: "d" (previously index 3) moves into
        // slot 1, so its index must be re-patched to 1.
        set.remove(1);
        assert_eq!(d.load(Ordering::Relaxed), 1, "replacer was not re-indexed");
        assert_eq!(a.load(Ordering::Relaxed), 0);
        assert_eq!(c.load(Ordering::Relaxed), 2);
        let mut seen = Vec::new();
        for v in set.values_mut() {
            seen.push(*v);
        }
        assert_eq!(seen, ["a", "d", "c"]);
        assert_eq!(set.len(), 3);
        assert_eq!(b.load(Ordering::Relaxed), 1, "the removed entry's index is stale but unused");

        // A subsequent remove through the patched index still targets "d".
        set.remove(1);
        let mut seen = Vec::new();
        for v in set.values_mut() {
            seen.push(*v);
        }
        assert_eq!(seen, ["a", "c"]);
    }
}
