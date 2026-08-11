use std::future::Future;

mod chain_selection;
mod conn;
mod conn_chain;
mod conn_selector;
mod degradation;
mod prober;
mod route_table;
mod rtt_stats;
pub use conn::*;
pub use conn_chain::*;
pub use conn_selector::*;
pub use prober::{DisplayChain, ProbeOutcome, ProbeRtt};
pub use route_table::*;

/// Probe futures collected during route/selector preparation.
///
/// Probe tasks are NOT started during prepare: the futures are collected
/// here and spawned into the server-owned `JoinSet` only at commit time, so
/// a failed or abandoned prepare can never leave a running probe task whose
/// panic would be lost (dropping this just drops unspawned futures).
#[derive(Default)]
pub struct ProbeFutures {
    futures: Vec<std::pin::Pin<Box<dyn Future<Output = ()> + Send + 'static>>>,
}
impl ProbeFutures {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn push(&mut self, fut: impl Future<Output = ()> + Send + 'static) {
        self.futures.push(Box::pin(fut));
    }
    pub fn is_empty(&self) -> bool {
        self.futures.is_empty()
    }
    pub fn into_futures(self) -> Vec<std::pin::Pin<Box<dyn Future<Output = ()> + Send + 'static>>> {
        self.futures
    }
}
