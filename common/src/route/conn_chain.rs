use std::{
    sync::{Arc, RwLock},
    time::Duration,
};

use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

use crate::{config::SharableConfig, header::route::RouteRequest, proto::addr::RouteAddr};

use super::{
    ConnConfig, ConnConfigBuildError, ProbeRtt, Registries, prober::probe_task, rtt_stats::RttStats,
};

pub const PROBE_ROUND_INTERVAL: Duration = Duration::from_secs(30);

pub type ConnChain = [ConnConfig];

/// # Panic
///
/// `nodes` must not be empty.
pub fn convert_proxies_to_header_crypto_pairs(
    nodes: &ConnChain,
    destination: Option<RouteAddr>,
) -> Vec<(RouteRequest<RouteAddr>, &tokio_chacha20::config::Config)> {
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
pub struct WeightedConnChainBuilder {
    pub weight: usize,
    pub chain: Vec<SharableConfig<ConnConfig>>,
}
impl WeightedConnChainBuilder {
    pub fn resolve(
        self,
        registries: &Registries<'_>,
    ) -> Result<WeightedConnChain, WeightedConnChainBuildError> {
        let chain = self
            .chain
            .into_iter()
            .map(|c| match c {
                SharableConfig::SharingKey(k) => registries
                    .conn
                    .get(&k)
                    .cloned()
                    .ok_or(WeightedConnChainBuildError::ProxyServerKeyNotFound(k)),
                SharableConfig::Private(c) => Ok(c),
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
pub struct WeightedConnChain {
    pub weight: usize,
    pub chain: Arc<ConnChain>,
    pub payload_crypto: Option<tokio_chacha20::config::Config>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ProbeTaskState {
    Disabled,
    Running,
    Cancelled,
    CompletedUnexpectedly,
    Panicked,
    JoinFailed,
}

#[derive(Debug)]
pub struct GaugedConnChain {
    weighted: WeightedConnChain,
    rtt_stats: Arc<RwLock<RttStats>>,
    loss: Arc<RwLock<Option<f64>>>,
    _probe_supervision: tokio::task::JoinSet<()>,
    probe_state: watch::Receiver<ProbeTaskState>,
}
impl GaugedConnChain {
    pub fn new(
        weighted: WeightedConnChain,
        tracer: Option<Arc<dyn ProbeRtt + Send + Sync>>,
        cancellation: CancellationToken,
    ) -> Self {
        let rtt_stats = Arc::new(RwLock::new(RttStats::default()));
        let loss = Arc::new(RwLock::new(None));
        let (probe_state_tx, probe_state) = watch::channel(ProbeTaskState::Disabled);
        let mut _probe_supervision = tokio::task::JoinSet::new();
        if let Some(tracer) = tracer {
            probe_state_tx.send(ProbeTaskState::Running).ok();
            let probe_cancellation = cancellation.clone();
            let probe_state_tx = probe_state_tx.clone();
            let chain = weighted.chain.clone();
            let rtt_stats = rtt_stats.clone();
            let loss = loss.clone();
            _probe_supervision.spawn(async move {
                let mut probes = tokio::task::JoinSet::new();
                probes.spawn(probe_task(
                    tracer,
                    chain,
                    rtt_stats,
                    loss,
                    probe_cancellation.clone(),
                ));
                match probes.join_next().await {
                    Some(Ok(())) if probe_cancellation.is_cancelled() => {
                        tracing::debug!("Route probe task cancelled");
                        probe_state_tx.send(ProbeTaskState::Cancelled).ok();
                    }
                    Some(Ok(())) => {
                        tracing::error!("Route probe task completed unexpectedly");
                        probe_state_tx
                            .send(ProbeTaskState::CompletedUnexpectedly)
                            .ok();
                    }
                    Some(Err(error)) if error.is_panic() => {
                        tracing::error!(?error, "Route probe task panicked");
                        probe_state_tx.send(ProbeTaskState::Panicked).ok();
                    }
                    Some(Err(error)) => {
                        tracing::error!(?error, "Route probe task failed to join");
                        probe_state_tx.send(ProbeTaskState::JoinFailed).ok();
                    }
                    None => {}
                }
            });
        }
        Self {
            weighted,
            rtt_stats,
            loss,
            _probe_supervision,
            probe_state,
        }
    }

    pub fn weighted(&self) -> &WeightedConnChain {
        &self.weighted
    }

    pub fn rtt(&self) -> Option<Duration> {
        self.rtt_stats.read().unwrap().srtt
    }

    pub fn rttvar(&self) -> Option<Duration> {
        self.rtt_stats.read().unwrap().rttvar
    }

    /// `srtt + 2*rttvar` (the RFC 6298-style effective RTT used for chain scoring).
    pub fn rtt_eff(&self) -> Option<Duration> {
        self.rtt_stats.read().unwrap().effective()
    }

    pub fn loss(&self) -> Option<f64> {
        *self.loss.read().unwrap()
    }

    #[cfg(test)]
    fn probe_state(&self) -> ProbeTaskState {
        *self.probe_state.borrow()
    }

    #[cfg(test)]
    pub(crate) fn set_gauges_for_test(&self, rtt_sample: Option<Duration>, loss: Option<f64>) {
        if let Some(rtt_sample) = rtt_sample {
            self.rtt_stats.write().unwrap().apply_sample(rtt_sample);
        }
        *self.loss.write().unwrap() = loss;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        pin::Pin,
        sync::atomic::{AtomicUsize, Ordering},
    };

    use crate::error::AnyError;

    struct CountingTracer(Arc<AtomicUsize>);
    impl ProbeRtt for CountingTracer {
        fn probe_rtt(
            &self,
            _chain: &ConnChain,
        ) -> Pin<Box<dyn Future<Output = Result<Duration, AnyError>> + Send + '_>> {
            Box::pin(async move {
                self.0.fetch_add(1, Ordering::SeqCst);
                Ok(Duration::from_millis(1))
            })
        }
    }

    #[tokio::test]
    async fn dropping_the_chain_aborts_the_probe_task() {
        let counter = Arc::new(AtomicUsize::new(0));
        let chain = GaugedConnChain::new(
            WeightedConnChain {
                weight: 1,
                chain: Arc::from(Vec::<crate::route::ConnConfig>::new()),
                payload_crypto: None,
            },
            Some(Arc::new(CountingTracer(counter.clone())) as Arc<dyn ProbeRtt + Send + Sync>),
            CancellationToken::new(),
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
        let before = counter.load(Ordering::SeqCst);
        assert!(before >= 1, "the probe task should have run");
        drop(chain);
        tokio::time::sleep(Duration::from_millis(200)).await;
        let after = counter.load(Ordering::SeqCst);
        assert!(
            after <= before + 2,
            "probe task must be aborted by object drop (before={before}, after={after})"
        );
    }

    struct PanickingTracer;
    impl ProbeRtt for PanickingTracer {
        fn probe_rtt(
            &self,
            _chain: &ConnChain,
        ) -> Pin<Box<dyn Future<Output = Result<Duration, AnyError>> + Send + '_>> {
            Box::pin(async move {
                panic!("synthetic probe panic");
            })
        }
    }

    #[tokio::test]
    async fn probe_panic_is_observed_without_a_reap_call() {
        let chain = GaugedConnChain::new(
            WeightedConnChain {
                weight: 1,
                chain: Arc::from(Vec::<crate::route::ConnConfig>::new()),
                payload_crypto: None,
            },
            Some(Arc::new(PanickingTracer) as Arc<dyn ProbeRtt + Send + Sync>),
            CancellationToken::new(),
        );
        let deadline = std::time::Instant::now() + Duration::from_secs(1);
        while chain.probe_state() != ProbeTaskState::Panicked {
            assert!(
                std::time::Instant::now() < deadline,
                "probe panic was never observed"
            );
            tokio::task::yield_now().await;
        }
    }

    #[tokio::test]
    async fn probe_cancellation_is_classified() {
        let cancellation = CancellationToken::new();
        let chain = GaugedConnChain::new(
            WeightedConnChain {
                weight: 1,
                chain: Arc::from(Vec::<crate::route::ConnConfig>::new()),
                payload_crypto: None,
            },
            Some(Arc::new(CountingTracer(Arc::new(AtomicUsize::new(0))))
                as Arc<dyn ProbeRtt + Send + Sync>),
            cancellation.clone(),
        );
        cancellation.cancel();
        let deadline = std::time::Instant::now() + Duration::from_secs(1);
        while chain.probe_state() != ProbeTaskState::Cancelled {
            assert!(
                std::time::Instant::now() < deadline,
                "probe cancellation was never classified"
            );
            tokio::task::yield_now().await;
        }
        drop(chain);
    }
}
