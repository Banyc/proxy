//! Thread-local metrics recorder for tests that need to read a named
//! counter's *live* value.
//!
//! A byte series that only moves when a stream ends cannot be asserted
//! against while the stream is open; this recorder captures every counter
//! registered through it, so a test can read how many bytes a still-running
//! relay has published so far.

use std::{
    collections::HashMap,
    sync::{Arc, Mutex, atomic::Ordering},
};

use metrics::{
    Counter, Gauge, Histogram, Key, KeyName, Metadata, Recorder, SharedString, Unit,
    atomics::AtomicU64,
};

/// Captures every counter registered through the recorder it backs.
#[derive(Default)]
pub struct LiveCounterRecorder {
    counters: Mutex<HashMap<String, Arc<AtomicU64>>>,
}

impl LiveCounterRecorder {
    /// The value `name` has accumulated so far; `0` when it has never been
    /// registered (which is itself a valid reading: nothing was published).
    pub fn counter_value(&self, name: &str) -> u64 {
        self.counters
            .lock()
            .unwrap()
            .get(name)
            .map(|counter| counter.load(Ordering::Relaxed))
            .unwrap_or(0)
    }
}

impl Recorder for LiveCounterRecorder {
    fn describe_counter(&self, _key: KeyName, _unit: Option<Unit>, _description: SharedString) {}
    fn describe_gauge(&self, _key: KeyName, _unit: Option<Unit>, _description: SharedString) {}
    fn describe_histogram(&self, _key: KeyName, _unit: Option<Unit>, _description: SharedString) {}

    fn register_counter(&self, key: &Key, _metadata: &Metadata<'_>) -> Counter {
        let mut counters = self.counters.lock().unwrap();
        let counter = counters.entry(key.name().to_string()).or_default().clone();
        Counter::from_arc(counter)
    }

    fn register_gauge(&self, _key: &Key, _metadata: &Metadata<'_>) -> Gauge {
        Gauge::noop()
    }

    fn register_histogram(&self, _key: &Key, _metadata: &Metadata<'_>) -> Histogram {
        Histogram::noop()
    }
}
