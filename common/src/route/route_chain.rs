use std::{
    sync::{Arc, RwLock},
    time::Duration,
};

use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

use crate::{config::SharableConfig, header::route::RouteRequest, proxy_runtime::addr::RouteAddr};

use super::{
    HopConfig, HopConfigBuildError, ProbeFutures, ProbeRtt, Registries, prober::probe_task,
    rtt_stats::RttStats,
};

pub const PROBE_ROUND_INTERVAL: Duration = Duration::from_secs(30);

pub type RouteChain = [HopConfig];

/// # Panic
///
/// `nodes` must not be empty.
pub fn convert_proxies_to_header_crypto_pairs(
    nodes: &RouteChain,
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
pub struct WeightedRouteChainBuilder {
    pub weight: usize,
    pub chain: Vec<SharableConfig<HopConfig>>,
}
impl WeightedRouteChainBuilder {
    pub fn resolve(
        self,
        registries: &Registries<'_>,
    ) -> Result<WeightedRouteChain, WeightedRouteChainBuildError> {
        let chain = self
            .chain
            .into_iter()
            .map(|c| -> Result<HopConfig, WeightedRouteChainBuildError> {
                match c {
                    SharableConfig::SharingKey(k) => {
                        let mut conn = registries.conn.get(&k).cloned().ok_or(
                            WeightedRouteChainBuildError::ProxyServerKeyNotFound(k.clone()),
                        )?;
                        // Keep the config key as the conn's name so chain
                        // logs print the name instead of the raw address.
                        conn.name = Some(k);
                        Ok(conn)
                    }
                    SharableConfig::Private(c) => Ok(c),
                }
            })
            .collect::<Result<Arc<_>, _>>()?;
        Ok(WeightedRouteChain {
            weight: self.weight,
            chain,
        })
    }
}
#[derive(Debug, Error)]
pub enum WeightedRouteChainBuildError {
    #[error("{0}")]
    ProxyServer(#[from] HopConfigBuildError),
    #[error("Proxy server key not found: {0}")]
    ProxyServerKeyNotFound(Arc<str>),
}
#[derive(Debug)]
pub struct WeightedRouteChain {
    pub weight: usize,
    pub chain: Arc<RouteChain>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) enum ProbeTaskState {
    Disabled,
    Running,
    Cancelled,
    CompletedUnexpectedly,
    Panicked,
    JoinFailed,
}

impl ProbeTaskState {
    /// Whether the probe is alive or was never started / intentionally stopped.
    ///
    /// `Disabled` — no probe configured; gauges are neutral (`None`).
    /// `Running` — probe is actively updating gauges.
    /// `Cancelled` — the generation token was intentionally cancelled.
    ///
    /// All other variants are terminal failures: the probe died without
    /// an explicit cancel, so its gauges are frozen and stale.
    fn is_healthy(self) -> bool {
        matches!(self, Self::Disabled | Self::Running | Self::Cancelled)
    }
}

#[derive(Debug)]
pub struct GaugedRouteChain {
    weighted: WeightedRouteChain,
    rtt_stats: Arc<RwLock<RttStats>>,
    loss: Arc<RwLock<Option<f64>>>,
    probe_state: watch::Receiver<ProbeTaskState>,
    #[cfg(test)]
    probe_state_tx: watch::Sender<ProbeTaskState>,
}
impl GaugedRouteChain {
    /// Return the chain handle.
    ///
    /// The probe future is collected into the caller-owned [`ProbeFutures`]
    /// collector (`probes`) rather than spawned: it is started only at the
    /// commit boundary, when the collected futures are spawned into the
    /// server-owned `JoinSet`, which is drained with `result.unwrap()` so
    /// panics propagate instead of being downgraded to a watch state. A
    /// failed or abandoned prepare drops only unspawned futures. The chain
    /// retains only the `probe_state` watch receiver for state observation.
    pub fn new(
        weighted: WeightedRouteChain,
        tracer: Option<Arc<dyn ProbeRtt + Send + Sync>>,
        cancellation: CancellationToken,
        probes: &mut ProbeFutures,
    ) -> Self {
        let rtt_stats = Arc::new(RwLock::new(RttStats::default()));
        let loss = Arc::new(RwLock::new(None));
        let (probe_state_tx, probe_state) = watch::channel(ProbeTaskState::Disabled);
        if let Some(tracer) = tracer {
            probe_state_tx.send(ProbeTaskState::Running).ok();
            let probe_cancellation = cancellation.clone();
            let probe_state_tx = probe_state_tx.clone();
            let chain = weighted.chain.clone();
            let rtt_stats = rtt_stats.clone();
            let loss = loss.clone();
            probes.push(async move {
                probe_task(tracer, chain, rtt_stats, loss, probe_cancellation.clone()).await;
                // `probe_task` only returns after its cancellation token
                // fires, so reaching here means the generation was cancelled.
                // Any panic inside `probe_task` propagates out of this future
                // and surfaces at the commit-time `JoinSet` reap (which
                // unwraps), rather than being downgraded to a watch state.
                probe_state_tx.send(ProbeTaskState::Cancelled).ok();
            });
        }
        Self {
            weighted,
            rtt_stats,
            loss,
            probe_state,
            #[cfg(test)]
            probe_state_tx,
        }
    }

    pub fn weighted(&self) -> &WeightedRouteChain {
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

    /// The RTT knowledge state for chain scoring.
    ///
    /// Returns [`RttSlot::Unreachable`] when the probe has died without an
    /// intentional cancel, so the frozen gauges are not used for routing.
    /// Otherwise returns `Measured` or `Unmeasured` depending on whether
    /// the probe has produced a sample.
    pub(crate) fn rtt_slot(&self) -> super::chain_selection::RttSlot {
        if !self.probe_healthy() {
            return super::chain_selection::RttSlot::Unreachable;
        }
        match self.rtt_eff() {
            Some(d) => super::chain_selection::RttSlot::Measured(d),
            None => super::chain_selection::RttSlot::Unmeasured,
        }
    }

    pub fn loss(&self) -> Option<f64> {
        *self.loss.read().unwrap()
    }

    pub(crate) fn probe_state(&self) -> ProbeTaskState {
        *self.probe_state.borrow()
    }

    /// Whether the probe is alive (or was never started / intentionally
    /// stopped).  When this returns `false` the RTT/loss gauges are frozen
    /// at whatever the probe last wrote and should not drive routing
    /// decisions.
    pub(crate) fn probe_healthy(&self) -> bool {
        self.probe_state().is_healthy()
    }

    #[cfg(test)]
    pub(crate) fn set_gauges_for_test(&self, rtt_sample: Option<Duration>, loss: Option<f64>) {
        if let Some(rtt_sample) = rtt_sample {
            self.rtt_stats.write().unwrap().apply_sample(rtt_sample);
        }
        *self.loss.write().unwrap() = loss;
    }

    #[cfg(test)]
    pub(crate) fn set_probe_state_for_test(&self, state: ProbeTaskState) {
        self.probe_state_tx.send(state).ok();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        connect::{ConnectorConfig, connector_config_cell},
        proxy_runtime::connect::stream::StreamConnectorTable,
        route::ProbeOutcome,
    };
    use std::{
        collections::HashMap,
        pin::Pin,
        sync::atomic::{AtomicUsize, Ordering},
    };

    struct CountingTracer(Arc<AtomicUsize>);
    impl ProbeRtt for CountingTracer {
        fn probe_rtt(
            &self,
            _chain: &RouteChain,
        ) -> Pin<Box<dyn Future<Output = ProbeOutcome> + Send + '_>> {
            Box::pin(async move {
                self.0.fetch_add(1, Ordering::SeqCst);
                ProbeOutcome {
                    rtt: Ok(Duration::from_millis(1)),
                    epilog: None,
                }
            })
        }
    }

    #[tokio::test]
    async fn dropping_the_chain_aborts_the_probe_task() {
        let counter = Arc::new(AtomicUsize::new(0));
        let mut probes = ProbeFutures::new();
        let chain = GaugedRouteChain::new(
            WeightedRouteChain {
                weight: 1,
                chain: Arc::from(Vec::<crate::route::HopConfig>::new()),
            },
            Some(Arc::new(CountingTracer(counter.clone())) as Arc<dyn ProbeRtt + Send + Sync>),
            CancellationToken::new(),
            &mut probes,
        );
        // The probe future is collected during prepare; spawn it into a local
        // JoinSet (as commit does) so the probe actually runs.
        let mut generation = tokio::task::JoinSet::new();
        for fut in probes.into_futures() {
            generation.spawn(fut);
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
        let before = counter.load(Ordering::SeqCst);
        assert!(before >= 1, "the probe task should have run");
        // The probe is owned by the generation JoinSet; dropping it aborts
        // the probe task.
        drop(generation);
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
            _chain: &RouteChain,
        ) -> Pin<Box<dyn Future<Output = ProbeOutcome> + Send + '_>> {
            Box::pin(async move {
                panic!("synthetic probe panic");
            })
        }
    }

    #[tokio::test]
    #[should_panic(expected = "synthetic probe panic")]
    async fn probe_panic_is_observed_at_the_generation_boundary() {
        let mut probes = ProbeFutures::new();
        let _chain = GaugedRouteChain::new(
            WeightedRouteChain {
                weight: 1,
                chain: Arc::from(Vec::<crate::route::HopConfig>::new()),
            },
            Some(Arc::new(PanickingTracer) as Arc<dyn ProbeRtt + Send + Sync>),
            CancellationToken::new(),
            &mut probes,
        );
        // The probe task is spawned at the commit boundary from the
        // collected future; joining it re-raises the probe panic with its
        // original message.
        let mut generation = tokio::task::JoinSet::new();
        for fut in probes.into_futures() {
            generation.spawn(fut);
        }
        generation.join_next().await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn probe_cancellation_is_classified() {
        let cancellation = CancellationToken::new();
        let mut probes = ProbeFutures::new();
        let chain = GaugedRouteChain::new(
            WeightedRouteChain {
                weight: 1,
                chain: Arc::from(Vec::<crate::route::HopConfig>::new()),
            },
            Some(Arc::new(CountingTracer(Arc::new(AtomicUsize::new(0))))
                as Arc<dyn ProbeRtt + Send + Sync>),
            cancellation.clone(),
            &mut probes,
        );
        // Spawn the collected future so the probe task runs and can observe
        // cancellation.
        let mut generation = tokio::task::JoinSet::new();
        for fut in probes.into_futures() {
            generation.spawn(fut);
        }
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

    #[test]
    fn is_healthy_classifies_terminal_failures_as_unhealthy() {
        assert!(ProbeTaskState::Disabled.is_healthy());
        assert!(ProbeTaskState::Running.is_healthy());
        assert!(ProbeTaskState::Cancelled.is_healthy());
        assert!(!ProbeTaskState::CompletedUnexpectedly.is_healthy());
        assert!(!ProbeTaskState::Panicked.is_healthy());
        assert!(!ProbeTaskState::JoinFailed.is_healthy());
    }

    #[test]
    fn probe_healthy_reflects_injected_state() {
        let mut probes = ProbeFutures::new();
        let chain = GaugedRouteChain::new(
            WeightedRouteChain {
                weight: 1,
                chain: Arc::from(Vec::<crate::route::HopConfig>::new()),
            },
            None,
            CancellationToken::new(),
            &mut probes,
        );
        assert!(chain.probe_healthy(), "Disabled is healthy");
        chain.set_probe_state_for_test(ProbeTaskState::Panicked);
        assert!(
            !chain.probe_healthy(),
            "Panicked must be reflected as unhealthy"
        );
        chain.set_probe_state_for_test(ProbeTaskState::Cancelled);
        assert!(
            chain.probe_healthy(),
            "Cancelled must be reflected as healthy"
        );
    }

    #[test]
    fn a_chain_resolved_by_key_keeps_the_conn_name() {
        struct NoTracer;
        impl ProbeRtt for NoTracer {
            fn probe_rtt(
                &self,
                _chain: &RouteChain,
            ) -> Pin<Box<dyn Future<Output = ProbeOutcome> + Send + '_>> {
                unreachable!()
            }
        }
        let conn: HashMap<Arc<str>, HopConfig> = HashMap::from([(
            Arc::from("tcp1"),
            HopConfig {
                name: None,
                address: "tcp://127.0.0.1:1".parse().unwrap(),
                header_crypto: tokio_chacha20::config::Config::new(
                    [7; tokio_chacha20::KEY_BYTES].into(),
                ),
                payload_crypto: None,
            },
        )]);
        let matcher = Arc::new(HashMap::new());
        let conn_selector = HashMap::new();
        let tracer: Arc<dyn ProbeRtt + Send + Sync> = Arc::new(NoTracer);
        let connector_table = Arc::new(StreamConnectorTable::new(
            connector_config_cell(ConnectorConfig::default()).0,
            HashMap::new(),
        ));
        let registries = Registries {
            conn: &conn,
            matcher: &matcher,
            conn_selector: &conn_selector,
            tracer: &tracer,
            connector_table: &connector_table,
            cancellation: CancellationToken::new(),
        };
        let chain = WeightedRouteChainBuilder {
            weight: 1,
            chain: vec![SharableConfig::SharingKey("tcp1".into())],
        }
        .resolve(&registries)
        .unwrap();
        assert_eq!(chain.chain[0].name.as_deref(), Some("tcp1"));
        assert_eq!(chain.chain[0].to_string(), "tcp1");
    }
}
