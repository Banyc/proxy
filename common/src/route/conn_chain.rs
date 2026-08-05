use std::{
    sync::{Arc, RwLock},
    time::Duration,
};

use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio_util::sync::CancellationToken;

use crate::{config::SharableConfig, header::route::RouteRequest, proto::addr::RouteAddr};

use super::{
    ConnConfig, ConnConfigBuildError, ProbeRtt, Registries, prober::probe_task,
    rtt_stats::RttStats,
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

#[derive(Debug)]
pub struct GaugedConnChain {
    weighted: WeightedConnChain,
    rtt_stats: Arc<RwLock<RttStats>>,
    loss: Arc<RwLock<Option<f64>>>,
    tasks: tokio::task::JoinSet<()>,
}
impl GaugedConnChain {
    pub fn new(
        weighted: WeightedConnChain,
        tracer: Option<Arc<dyn ProbeRtt + Send + Sync>>,
        cancellation: CancellationToken,
    ) -> Self {
        let rtt_stats = Arc::new(RwLock::new(RttStats::default()));
        let loss = Arc::new(RwLock::new(None));
        let mut tasks = tokio::task::JoinSet::new();
        if let Some(tracer) = tracer {
            tasks.spawn(probe_task(
                tracer,
                weighted.chain.clone(),
                rtt_stats.clone(),
                loss.clone(),
                cancellation,
            ));
        }
        Self {
            weighted,
            rtt_stats,
            loss,
            tasks,
        }
    }

    /// Observe probe task exits while the object is alive.
    pub fn reap(&mut self) {
        while let Some(res) = self.tasks.try_join_next() {
            match res {
                Ok(()) => {}
                Err(error) if error.is_panic() => {
                    tracing::error!(?error, "Route probe task panicked");
                    std::panic::resume_unwind(error.into_panic());
                }
                Err(error) => {
                    tracing::error!(?error, "Route probe task failed to join");
                }
            }
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
}
