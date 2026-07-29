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

use super::{ConnConfig, ConnConfigBuildError, ConnConfigBuilder, IntoAddr};

pub const TRACE_INTERVAL: Duration = Duration::from_secs(30);
const TRACE_DEAD_INTERVAL: Duration = Duration::from_secs(60 * 2);
const TRACES_PER_WAVE: usize = 5;
const INTRA_WAVE_GAP: Duration = Duration::from_millis(200);
const RTT_TIMEOUT: Duration = Duration::from_secs(5);
const RTT_EWMA_ALPHA: f64 = 0.3;
const LOSS_EWMA_ALPHA_UP: f64 = 0.7;
const LOSS_EWMA_ALPHA_DOWN: f64 = 0.3;

fn median_duration(rtts: &mut [Duration]) -> Option<Duration> {
    if rtts.is_empty() { return None; }
    rtts.sort_unstable();
    let mid = rtts.len() / 2;
    Some(if rtts.len() % 2 == 1 { rtts[mid] } else { (rtts[mid - 1] + rtts[mid]) / 2 })
}

fn ewma_duration(prev: Option<Duration>, sample: Option<Duration>, alpha: f64) -> Option<Duration> {
    match (prev, sample) {
        (_, None) => prev,
        (None, Some(s)) => Some(s),
        (Some(p), Some(s)) => {
            let blended = p.as_secs_f64() * (1. - alpha) + s.as_secs_f64() * alpha;
            Some(Duration::from_secs_f64(blended))
        }
    }
}

fn ewma_loss(prev: Option<f64>, sample: f64) -> f64 {
    match prev {
        None => sample,
        Some(p) => {
            let alpha = if sample > p { LOSS_EWMA_ALPHA_UP } else { LOSS_EWMA_ALPHA_DOWN };
            p * (1. - alpha) + sample * alpha
        }
    }
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
    rtt: Arc<RwLock<Option<Duration>>>,
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
        let rtt = Arc::new(RwLock::new(None));
        let loss = Arc::new(RwLock::new(None));
        let task_handle = tracer.map(|tracer| {
            spawn_tracer(
                tracer,
                weighted.chain.clone(),
                rtt.clone(),
                loss.clone(),
                cancellation,
            )
        });
        Self {
            weighted,
            rtt,
            loss,
            task_handle,
        }
    }

    pub fn weighted(&self) -> &WeightedConnChain<Addr> {
        &self.weighted
    }

    pub fn rtt(&self) -> Option<Duration> {
        *self.rtt.read().unwrap()
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
    rtt_store: Arc<RwLock<Option<Duration>>>,
    loss_store: Arc<RwLock<Option<f64>>>,
    cancellation: CancellationToken,
) -> tokio::task::JoinHandle<()>
where
    Tracer: TraceRtt<Addr = Addr> + Send + Sync + 'static,
    Addr: fmt::Display + Send + Sync + 'static,
{
    tokio::task::spawn(async move {
        let mut ping: tokio::task::JoinSet<Option<Duration>> = tokio::task::JoinSet::new();
        while !cancellation.is_cancelled() {
            let mut rtts = Vec::with_capacity(TRACES_PER_WAVE);
            for i in 0..TRACES_PER_WAVE {
                if cancellation.is_cancelled() { break; }
                let chain = chain.clone();
                let tracer = tracer.clone();
                ping.spawn(async move {
                    match tokio::time::timeout(RTT_TIMEOUT, tracer.trace_rtt(&chain)).await {
                        Ok(Ok(rtt)) => Some(rtt),
                        Ok(Err(e)) => { trace!("trace error: {e:?}"); None }
                        Err(_) => { trace!("trace timeout"); None }
                    }
                });
                match ping.join_next().await {
                    Some(Ok(Some(rtt))) => rtts.push(rtt),
                    Some(Ok(None)) => {}
                    Some(Err(e)) => trace!("ping task join error: {e:?}"),
                    None => {}
                }
                if i + 1 < TRACES_PER_WAVE {
                    tokio::select! {
                        () = tokio::time::sleep(INTRA_WAVE_GAP) => {}
                        () = cancellation.cancelled() => break
                    }
                }
            }
            if cancellation.is_cancelled() { break; }
            let ok = rtts.len();
            let wave_rtt = median_duration(&mut rtts);
            let wave_loss = (TRACES_PER_WAVE - ok) as f64 / TRACES_PER_WAVE as f64;
            let rtt = {
                let mut store = rtt_store.write().unwrap();
                *store = ewma_duration(*store, wave_rtt, RTT_EWMA_ALPHA);
                *store
            };
            let loss = {
                let mut store = loss_store.write().unwrap();
                *store = Some(ewma_loss(*store, wave_loss));
                *store
            };
            let addresses = DisplayChain(&chain);
            info!(%addresses, wave_rtt = ?wave_rtt, wave_loss, rtt = ?rtt, ?loss, "Traced RTT");
            let idle = if ok == 0 { TRACE_DEAD_INTERVAL } else { TRACE_INTERVAL };
            tokio::select! {
                () = tokio::time::sleep(idle) => {}
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
    fn ms(n: u64) -> Duration { Duration::from_millis(n) }
    #[test]
    fn median_is_outlier_robust() {
        assert_eq!(median_duration(&mut [ms(10), ms(20), ms(5000)]), Some(ms(20)));
    }
    #[test]
    fn median_even_count_averages_the_two_middles() {
        assert_eq!(median_duration(&mut [ms(40), ms(10), ms(30), ms(20)]), Some(ms(25)));
    }
    #[test]
    fn median_of_empty_wave_is_none() {
        assert_eq!(median_duration(&mut []), None);
    }
    #[test]
    fn ewma_duration_first_sample_seeds_the_estimate() {
        assert_eq!(ewma_duration(None, Some(ms(100)), 0.3), Some(ms(100)));
    }
    #[test]
    fn ewma_duration_dead_wave_keeps_previous_estimate() {
        assert_eq!(ewma_duration(Some(ms(100)), None, 0.3), Some(ms(100)));
        assert_eq!(ewma_duration(None, None, 0.3), None);
    }
    #[test]
    fn ewma_duration_blends_toward_the_new_sample() {
        let blended = ewma_duration(Some(ms(100)), Some(ms(200)), 0.3).unwrap();
        assert!((blended.as_secs_f64() - 0.130).abs() < 1e-9);
    }
    #[test]
    fn ewma_loss_is_fast_to_distrust_slow_to_trust() {
        assert!((ewma_loss(None, 0.4) - 0.4).abs() < f64::EPSILON);
        let up = ewma_loss(Some(0.2), 1.0);
        assert!((up - 0.76).abs() < 1e-9);
        let down = ewma_loss(Some(0.8), 0.0);
        assert!((down - 0.56).abs() < 1e-9);
        let up_move = ewma_loss(Some(0.5), 1.0) - 0.5;
        let down_move = 0.5 - ewma_loss(Some(0.5), 0.0);
        assert!(up_move > down_move);
        assert!((0.0..=1.0).contains(&up) && (0.0..=1.0).contains(&down));
    }
}
