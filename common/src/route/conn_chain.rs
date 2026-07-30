use std::{
    collections::HashMap,
    fmt,
    sync::{Arc, RwLock},
    time::Duration,
};

use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio_util::sync::CancellationToken;
use tracing::{info, trace};

use crate::{config::SharableConfig, error::AnyError, header::route::RouteRequest};

use super::{
    score::{RttStats, ewma_loss},
    ConnConfig, ConnConfigBuildError, ConnConfigBuilder, IntoAddr,
};

pub const TRACE_INTERVAL: Duration = Duration::from_secs(30);
const TRACE_DEAD_INTERVAL: Duration = Duration::from_secs(60 * 2);
const PROBES_PER_INTERVAL: u32 = 5;
const PROBE_MEAN_INTERVAL: Duration = Duration::from_millis(TRACE_INTERVAL.as_millis() as u64 / PROBES_PER_INTERVAL as u64);
const PROBE_MEAN_INTERVAL_DEAD: Duration = Duration::from_millis(TRACE_DEAD_INTERVAL.as_millis() as u64 / PROBES_PER_INTERVAL as u64);
const PROBE_MIN_INTERVAL: Duration = Duration::from_millis(500);
const PROBE_MAX_INTERVAL: Duration = Duration::from_secs(60);
const DEAD_CONSECUTIVE_FAILURES: u32 = 5;
const RTT_TIMEOUT: Duration = Duration::from_secs(5);

fn poisson_interval(mean: Duration) -> Duration {
    let u: f64 = rand::random::<f64>().clamp(f64::MIN_POSITIVE, 1.0);
    let d = Duration::from_secs_f64(mean.as_secs_f64() * -u.ln());
    d.clamp(PROBE_MIN_INTERVAL, PROBE_MAX_INTERVAL)
}

pub type ConnChain<Addr> = [ConnConfig<Addr>];

/// # Panic
///
/// `nodes` must not be empty.
pub fn convert_proxies_to_header_crypto_pairs<Addr>(
    nodes: &ConnChain<Addr>,
    destination: Option<Addr>,
) -> Vec<(RouteRequest<Addr>, &tokio_chacha20::config::Config)>
where
    Addr: Clone + Sync + Send,
{
    assert!(!nodes.is_empty());
    let mut pairs = (0..nodes.len() - 1)
        .map(|i| {
            let node = &nodes[i];
            let next_node = &nodes[i + 1];
            let route_req = RouteRequest {
                upstream: Some(next_node.address.clone()),
            };
            (route_req, &node.header_crypto)
        })
        .collect::<Vec<_>>();
    let route_req = RouteRequest {
        upstream: destination,
    };
    pairs.push((route_req, &nodes.last().unwrap().header_crypto));
    pairs
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WeightedConnChainBuilder<AddrStr> {
    pub weight: usize,
    pub chain: Vec<SharableConfig<ConnConfigBuilder<AddrStr>>>,
}
impl<AddrStr> WeightedConnChainBuilder<AddrStr> {
    pub fn build<Addr: Clone>(
        self,
        conn: &HashMap<Arc<str>, ConnConfig<Addr>>,
    ) -> Result<WeightedConnChain<Addr>, WeightedConnChainBuildError>
    where
        AddrStr: IntoAddr<Addr = Addr>,
    {
        let chain = self
            .chain
            .into_iter()
            .map(|c| match c {
                SharableConfig::SharingKey(k) => conn
                    .get(&k)
                    .cloned()
                    .ok_or(WeightedConnChainBuildError::ProxyServerKeyNotFound(k)),
                SharableConfig::Private(c) => c.build().map_err(Into::into),
            })
            .collect::<Result<Arc<_>, _>>()?;
        let mut payload_crypto = None;
        for proxy_config in chain.iter() {
            let Some(p) = &proxy_config.payload_crypto else {
                continue;
            };
            if payload_crypto.is_some() {
                return Err(WeightedConnChainBuildError::MultiplePayloadKeys);
            }
            payload_crypto = Some(p.clone());
        }
        Ok(WeightedConnChain {
            weight: self.weight,
            chain,
            payload_crypto,
        })
    }
}
#[derive(Debug, Error)]
pub enum WeightedConnChainBuildError {
    #[error("{0}")]
    ProxyServer(#[from] ConnConfigBuildError),
    #[error("Proxy server key not found: {0}")]
    ProxyServerKeyNotFound(Arc<str>),
    #[error("Multiple payload keys")]
    MultiplePayloadKeys,
}

#[derive(Debug)]
pub struct WeightedConnChain<Addr> {
    pub weight: usize,
    pub chain: Arc<ConnChain<Addr>>,
    pub payload_crypto: Option<tokio_chacha20::config::Config>,
}

#[derive(Debug)]
pub struct GaugedConnChain<Addr> {
    weighted: WeightedConnChain<Addr>,
    rtt_stats: Arc<RwLock<RttStats>>,
    loss: Arc<RwLock<Option<f64>>>,
    task_handle: Option<tokio::task::JoinHandle<()>>,
}
impl<Addr> GaugedConnChain<Addr>
where
    Addr: std::fmt::Debug + fmt::Display + Clone + Send + Sync + 'static,
{
    pub fn new<T>(
        weighted: WeightedConnChain<Addr>,
        tracer: Option<Arc<T>>,
        cancellation: CancellationToken,
    ) -> Self
    where
        T: TraceRtt<Addr = Addr> + Send + Sync + 'static,
    {
        let rtt_stats = Arc::new(RwLock::new(RttStats::default()));
        let loss = Arc::new(RwLock::new(None));
        let task_handle = tracer.map(|tracer| {
            spawn_tracer(
                tracer,
                weighted.chain.clone(),
                rtt_stats.clone(),
                loss.clone(),
                cancellation,
            )
        });
        Self {
            weighted,
            rtt_stats,
            loss,
            task_handle,
        }
    }

    pub fn weighted(&self) -> &WeightedConnChain<Addr> {
        &self.weighted
    }

    pub fn rtt(&self) -> Option<Duration> {
        self.rtt_stats.read().unwrap().srtt
    }

    pub fn rttvar(&self) -> Option<Duration> {
        self.rtt_stats.read().unwrap().rttvar
    }

    pub fn rtt_eff(&self) -> Option<Duration> {
        self.rtt_stats.read().unwrap().effective()
    }

    pub fn loss(&self) -> Option<f64> {
        *self.loss.read().unwrap()
    }
}
impl<Addr> Drop for GaugedConnChain<Addr> {
    fn drop(&mut self) {
        if let Some(h) = self.task_handle.as_ref() {
            h.abort()
        }
    }
}

fn spawn_tracer<Tracer, Addr>(
    tracer: Arc<Tracer>,
    chain: Arc<ConnChain<Addr>>,
    rtt_stats_store: Arc<RwLock<RttStats>>,
    loss_store: Arc<RwLock<Option<f64>>>,
    cancellation: CancellationToken,
) -> tokio::task::JoinHandle<()>
where
    Tracer: TraceRtt<Addr = Addr> + Send + Sync + 'static,
    Addr: fmt::Display + Send + Sync + 'static,
{
    tokio::task::spawn(async move {
        let mut consecutive_failures: u32 = 0;
        let mut probes_since_log: u32 = 0;
        while !cancellation.is_cancelled() {
            let sample = match tokio::time::timeout(RTT_TIMEOUT, tracer.trace_rtt(&chain)).await {
                Ok(Ok(rtt)) => Some(rtt),
                Ok(Err(e)) => { trace!("trace error: {e:?}"); None }
                Err(_) => { trace!("trace timeout"); None }
            };
            if cancellation.is_cancelled() { break; }
            let (rtt, rttvar, rtt_eff) = {
                let mut store = rtt_stats_store.write().unwrap();
                if let Some(sample) = sample { store.apply_sample(sample); }
                (store.srtt, store.rttvar, store.effective())
            };
            let loss = {
                let mut store = loss_store.write().unwrap();
                *store = Some(ewma_loss(*store, if sample.is_some() { 0. } else { 1. }));
                *store
            };
            consecutive_failures = if sample.is_some() { 0 } else { consecutive_failures.saturating_add(1) };
            probes_since_log += 1;
            if probes_since_log >= PROBES_PER_INTERVAL {
                probes_since_log = 0;
                let addresses = DisplayChain(&chain);
                info!(%addresses, sample = ?sample, rtt = ?rtt, rttvar = ?rttvar, rtt_eff = ?rtt_eff, ?loss, "Traced RTT");
            }
            let mean = if consecutive_failures >= DEAD_CONSECUTIVE_FAILURES { PROBE_MEAN_INTERVAL_DEAD } else { PROBE_MEAN_INTERVAL };
            tokio::select! {
                () = tokio::time::sleep(poisson_interval(mean)) => {}
                () = cancellation.cancelled() => {}
            }
        }
    })
}

pub struct DisplayChain<'chain, Addr>(&'chain ConnChain<Addr>);
impl<Addr> fmt::Display for DisplayChain<'_, Addr>
where
    Addr: fmt::Display,
{
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "[")?;
        for (i, c) in self.0.iter().enumerate() {
            write!(f, "{}", c.address)?;
            if i + 1 != self.0.len() {
                write!(f, ",")?;
            }
        }
        write!(f, "]")?;
        Ok(())
    }
}

pub trait TraceRtt {
    type Addr;
    fn trace_rtt(
        &self,
        chain: &ConnChain<Self::Addr>,
    ) -> impl Future<Output = Result<Duration, AnyError>> + Send;
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn poisson_interval_respects_clamp_bounds() {
        for _ in 0..1000 {
            let d = poisson_interval(PROBE_MEAN_INTERVAL);
            assert!((PROBE_MIN_INTERVAL..=PROBE_MAX_INTERVAL).contains(&d), "{d:?}");
        }
    }
}
