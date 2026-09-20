//! Test-only allocation-counting instrument.
//!
//! A `#[global_allocator]` (installed under `cfg(test)` only, from the crate
//! root) that forwards to the system allocator while counting allocations on
//! the *current thread* whenever a [`Count`] scope is open on that thread.
//! The thread-local scoping is what makes this deterministically usable in a
//! concurrent test binary: allocations made by other test threads (running
//! under the same global allocator) are never counted, so a measurement
//! cannot be polluted by a parallel test.
//!
//! Usage:
//!
//! ```ignore
//! let guard = crate::test_alloc::Count::begin();
//! run_something();
//! assert_eq!(guard.allocs(), 2, "…");
//! ```

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;

/// Per-thread measurement state. Kept in a single `thread_local!` block so a
/// thread touching it initialises exactly one TLS block (any initialisation
/// cost lands before a [`Count::begin`] baseline is captured, never inside a
/// measurement).
#[derive(Clone, Copy)]
struct State {
    scopes: u32,
    allocs: usize,
    bytes: usize,
}
thread_local! {
    static STATE: Cell<State> = const {
        Cell::new(State {
            scopes: 0,
            allocs: 0,
            bytes: 0,
        })
    };
}

/// The process-global allocator used in `common`'s test binaries. Counts only
/// on threads with an open [`Count`] scope; every other thread sees plain
/// `System` behaviour with no counter traffic.
pub struct CountingAllocator;

fn active() -> bool {
    STATE.with(|state| state.get().scopes > 0)
}
fn record(layout: Layout) {
    record_size(layout.size());
}
fn record_size(size: usize) {
    STATE.with(|cell| {
        let mut state = cell.get();
        state.allocs += 1;
        state.bytes += size;
        cell.set(state);
    });
}

unsafe impl GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let ptr = unsafe { System.alloc(layout) };
        if active() {
            record(layout);
        }
        ptr
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) }
    }
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        let new_ptr = unsafe { System.realloc(ptr, layout, new_size) };
        if active() {
            record_size(new_size);
        }
        new_ptr
    }
    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        let ptr = unsafe { System.alloc_zeroed(layout) };
        if active() {
            record(layout);
        }
        ptr
    }
}

/// An open measurement scope on the current thread. Dropping it closes the
/// scope; nested scopes on the same thread measure the *delta* from their
/// own baseline, so an inner measurement is unaffected by an outer one.
pub struct Count {
    base_allocs: usize,
    base_bytes: usize,
}

impl Count {
    /// Open a measurement scope on the current thread.
    pub fn begin() -> Self {
        let (base_allocs, base_bytes) = STATE.with(|cell| {
            let mut state = cell.get();
            state.scopes += 1;
            let base = (state.allocs, state.bytes);
            cell.set(state);
            base
        });
        Count {
            base_allocs,
            base_bytes,
        }
    }

    /// Heap allocations made on this thread since [`Count::begin`].
    pub fn allocs(&self) -> usize {
        STATE.with(|state| state.get().allocs) - self.base_allocs
    }

    /// Total bytes requested by those allocations.
    pub fn bytes(&self) -> usize {
        STATE.with(|state| state.get().bytes) - self.base_bytes
    }
}

impl Drop for Count {
    fn drop(&mut self) {
        STATE.with(|cell| {
            let mut state = cell.get();
            debug_assert!(state.scopes > 0, "unbalanced Count scope");
            state.scopes -= 1;
            cell.set(state);
        });
    }
}
