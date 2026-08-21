use std::{
    collections::HashMap,
    fmt,
    num::NonZeroUsize,
    sync::{Arc, RwLock},
};

use rand::RngExt;
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio_util::sync::CancellationToken;
use tracing::info;

use crate::{proto::connect::stream::StreamConnectorTable, ttl_cell::TtlCell};

use super::{
    GaugedRouteChain, HopConfig, PROBE_ROUND_INTERVAL, ProbeFutures, ProbeRtt, WeightedRouteChain,
    WeightedRouteChainBuildError, WeightedRouteChainBuilder,
    chain_selection::{EligibilityGate, ScoredChain, chain_score, pick_weighted},
};

/// The merged-config registries plus runtime handles the route builders resolve
/// their names against.
#[derive(Clone)]
pub struct Registries<'caller> {
    pub conn: &'caller HashMap<Arc<str>, HopConfig>,
    pub matcher: &'caller Arc<HashMap<Arc<str>, crate::matcher::Matcher>>,
    pub conn_selector: &'caller HashMap<Arc<str>, RouteSelector>,
    pub tracer: &'caller Arc<dyn ProbeRtt + Send + Sync>,
    pub connector_table: &'caller Arc<StreamConnectorTable>,
    pub cancellation: CancellationToken,
}

impl fmt::Debug for Registries<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Registries")
            .field("conn", &self.conn)
            .field("matcher", &self.matcher)
            .field("conn_selector", &self.conn_selector)
            .field("connector_table", &self.connector_table)
            .field("cancellation", &self.cancellation)
            .finish()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RouteSelectorBuilder {
    pub chains: Vec<WeightedRouteChainBuilder>,
    #[serde(default, alias = "trace_rtt")]
    pub probe_rtt: bool,
    pub active_chains: Option<NonZeroUsize>,
    #[serde(default)]
    pub max_rtt_ratio: Option<f64>,
}
impl RouteSelectorBuilder {
    pub fn resolve(
        self,
        registries: &Registries<'_>,
        probes: &mut ProbeFutures,
    ) -> Result<RouteSelector, RouteSelectorBuildError> {
        let chains = self
            .chains
            .into_iter()
            .map(|c| c.resolve(registries))
            .collect::<Result<_, _>>()
            .map_err(RouteSelectorBuildError::ChainConfig)?;
        let tracer = match self.probe_rtt {
            true => Some(registries.tracer.clone()),
            false => None,
        };
        RouteSelector::new(
            chains,
            tracer,
            self.active_chains,
            self.max_rtt_ratio,
            registries.cancellation.clone(),
            probes,
        )
        .map_err(Into::into)
    }
}
#[derive(Debug, Error)]
pub enum RouteSelectorBuildError {
    #[error("Chain config is invalid: {0}")]
    ChainConfig(#[source] WeightedRouteChainBuildError),
    #[error("{0}")]
    RouteSelector(#[from] RouteSelectorError),
}

#[derive(Debug, Clone)]
pub enum RouteSelector {
    Empty,
    Some(NonEmptyRouteSelector),
}
impl RouteSelector {
    /// Build the selector.
    ///
    /// Probe futures are collected into the caller-owned [`ProbeFutures`]
    /// collector (`probes`) rather than spawned: they are started only at the
    /// commit boundary, when they are spawned into the server-owned `JoinSet`,
    /// which is drained with `result.unwrap()` so probe panics propagate. A
    /// failed or abandoned prepare drops only unspawned futures.
    pub fn new(
        chains: Vec<WeightedRouteChain>,
        tracer: Option<Arc<dyn ProbeRtt + Send + Sync>>,
        active_chains: Option<NonZeroUsize>,
        max_rtt_ratio: Option<f64>,
        cancellation: CancellationToken,
        probes: &mut ProbeFutures,
    ) -> Result<Self, RouteSelectorError> {
        if chains.is_empty() {
            return Ok(Self::Empty);
        }
        let selector = NonEmptyRouteSelector::new(
            chains,
            tracer,
            active_chains,
            max_rtt_ratio,
            cancellation,
            probes,
        )?;
        Ok(Self::Some(selector))
    }
}

#[derive(Debug, Clone)]
pub struct NonEmptyRouteSelector {
    chains: Arc<[GaugedRouteChain]>,
    cum_weight: NonZeroUsize,
    score_store: Arc<RwLock<ScoreStore>>,
    active_chains: NonZeroUsize,
    gate: Option<EligibilityGate>,
    /// The probe kind for selector logs, e.g. `"udp"` or `"stream"`; the
    /// tracer's [`ProbeRtt::probe_kind`] captured at build time.
    probe_kind: &'static str,
}
impl NonEmptyRouteSelector {
    /// Build the selector.
    ///
    /// Each chain's probe supervision future is collected into the
    /// caller-owned [`ProbeFutures`] collector (`probes`) rather than
    /// spawned; the futures are spawned into the server-owned `JoinSet` only
    /// at the commit boundary.
    pub fn new(
        chains: Vec<WeightedRouteChain>,
        tracer: Option<Arc<dyn ProbeRtt + Send + Sync>>,
        active_chains: Option<NonZeroUsize>,
        max_rtt_ratio: Option<f64>,
        cancellation: CancellationToken,
        probes: &mut ProbeFutures,
    ) -> Result<Self, RouteSelectorError> {
        let cum_weight = chains.iter().map(|c| c.weight).sum();
        if cum_weight == 0 {
            return Err(RouteSelectorError::ZeroAccumulatedWeight);
        }
        let cum_weight = NonZeroUsize::new(cum_weight).unwrap();
        let gate = match max_rtt_ratio {
            Some(r) if r.is_finite() && r >= 1.0 => Some(EligibilityGate::new(r)),
            Some(r) => return Err(RouteSelectorError::InvalidMaxRttRatio(r)),
            None => None,
        };

        let active_chains = match active_chains {
            Some(active_chains) => {
                if active_chains.get() > chains.len() {
                    return Err(RouteSelectorError::TooManyActiveChains);
                }
                active_chains
            }
            None => NonZeroUsize::new(chains.len()).unwrap(),
        };

        let chains = chains
            .into_iter()
            .map(|c| GaugedRouteChain::new(c, tracer.clone(), cancellation.clone(), probes))
            .collect::<Arc<[_]>>();
        let score_store = Arc::new(RwLock::new(ScoreStore::new(None, PROBE_ROUND_INTERVAL)));
        Ok(Self {
            chains,
            cum_weight,
            score_store,
            active_chains,
            gate,
            probe_kind: tracer
                .as_ref()
                .map(|tracer| tracer.probe_kind())
                .unwrap_or("unknown"),
        })
    }

    pub fn choose_chain(&self) -> &WeightedRouteChain {
        if self.chains.len() == 1 {
            return self.chains[0].weighted();
        }
        let scores = self.score_store.read().unwrap().get().cloned();
        let scores = match scores {
            Some(scores) => scores,
            None => {
                let scores: Arc<[_]> = self.scores().into();
                info!(kind = self.probe_kind, ?scores, "Calculated scores");
                let sum = scores.iter().map(|(_, s)| *s).sum::<f64>();
                let scores = Scores { scores, sum };
                self.score_store.write().unwrap().set(scores.clone());
                scores
            }
        };
        let mut rng = rand::rng();
        if scores.sum == 0. {
            let i = rng.random_range(0..scores.scores.len());
            return self.chains[scores.scores[i].0].weighted();
        }
        let r = rng.random_range(0. ..scores.sum);
        let i = pick_weighted(&scores.scores, r);
        self.chains[i].weighted()
    }

    fn scores(&self) -> Vec<(usize, f64)> {
        let cum_weight = self.cum_weight.get() as f64;
        let mut scored: Vec<ScoredChain> = self
            .chains
            .iter()
            .enumerate()
            .map(|(index, c)| {
                let rtt = c.rtt_slot();
                let weight = c.weighted().weight as f64 / cum_weight;
                let score = chain_score(weight, c.loss(), rtt);
                ScoredChain { index, score, rtt }
            })
            .collect();
        if let Some(gate) = &self.gate {
            let dropped = gate.retain_eligible(&mut scored);
            if dropped > 0 {
                info!(
                    kind = self.probe_kind,
                    dropped,
                    eligible = scored.len(),
                    max_rtt_ratio = gate.max_ratio,
                    "RTT eligibility gate excluded slower routes"
                );
            }
        }
        let mut scores: Vec<(usize, f64)> =
            scored.into_iter().map(|c| (c.index, c.score)).collect();
        scores.sort_by(|(_, a), (_, b)| b.total_cmp(a));
        scores.truncate(self.active_chains.get());
        scores
    }
}
#[derive(Debug, Error, Clone)]
pub enum RouteSelectorError {
    #[error("Zero accumulated weight with chains")]
    ZeroAccumulatedWeight,
    #[error("The number of active chains is more than the number of chains")]
    TooManyActiveChains,
    #[error("max_rtt_ratio must be finite and >= 1.0, got {0}")]
    InvalidMaxRttRatio(f64),
}

type ScoreStore = TtlCell<Scores>;
#[derive(Debug, Clone)]
struct Scores {
    scores: Arc<[(usize, f64)]>,
    sum: f64,
}

#[cfg(test)]
mod tests {
    use super::super::route_chain::ProbeTaskState;
    use super::*;
    use std::time::Duration;
    fn chain(weight: usize) -> WeightedRouteChain {
        WeightedRouteChain {
            weight,
            chain: Arc::from(Vec::<HopConfig>::new()),
        }
    }
    #[test]
    fn a_zero_sum_falls_back_within_the_eligible_set() {
        let mut probes = ProbeFutures::new();
        let selector = NonEmptyRouteSelector::new(
            vec![chain(0), chain(5)],
            None::<Arc<dyn ProbeRtt + Send + Sync>>,
            None,
            Some(1.5),
            CancellationToken::new(),
            &mut probes,
        )
        .unwrap();
        selector.chains[0].set_gauges_for_test(Some(Duration::from_millis(10)), Some(0.));
        selector.chains[1].set_gauges_for_test(Some(Duration::from_millis(5000)), Some(0.));
        for _ in 0..200 {
            assert_eq!(
                selector.choose_chain().weight,
                0,
                "the gate excluded the slow chain; the fallback must not reinstate it"
            );
        }
    }

    #[test]
    fn a_dead_probe_chain_is_excluded_regardless_of_stale_gauges() {
        let mut probes = ProbeFutures::new();
        let selector = NonEmptyRouteSelector::new(
            vec![chain(1), chain(2)],
            None::<Arc<dyn ProbeRtt + Send + Sync>>,
            None,
            Some(1.5),
            CancellationToken::new(),
            &mut probes,
        )
        .unwrap();
        selector.chains[0].set_gauges_for_test(Some(Duration::from_millis(10)), Some(0.));
        selector.chains[0].set_probe_state_for_test(ProbeTaskState::Panicked);
        selector.chains[1].set_gauges_for_test(Some(Duration::from_millis(5000)), Some(0.));
        for _ in 0..200 {
            assert_eq!(
                selector.choose_chain().weight,
                2,
                "the dead-probe chain must never be selected while a healthy one exists"
            );
        }
    }

    #[test]
    fn all_dead_probes_fall_back_to_uniform_selection() {
        let mut probes = ProbeFutures::new();
        let selector = NonEmptyRouteSelector::new(
            vec![chain(1), chain(2)],
            None::<Arc<dyn ProbeRtt + Send + Sync>>,
            None,
            None,
            CancellationToken::new(),
            &mut probes,
        )
        .unwrap();
        selector.chains[0].set_gauges_for_test(Some(Duration::from_millis(10)), Some(0.));
        selector.chains[0].set_probe_state_for_test(ProbeTaskState::CompletedUnexpectedly);
        selector.chains[1].set_gauges_for_test(Some(Duration::from_millis(5000)), Some(0.));
        selector.chains[1].set_probe_state_for_test(ProbeTaskState::JoinFailed);

        let mut saw = [false; 3];
        for _ in 0..5000 {
            let w = selector.choose_chain().weight;
            saw[w] = true;
        }
        assert!(saw[1] && saw[2], "uniform fallback must reach both chains");
    }

    #[test]
    fn cancelled_probe_is_treated_as_healthy() {
        let mut probes = ProbeFutures::new();
        let selector = NonEmptyRouteSelector::new(
            vec![chain(1), chain(2)],
            None::<Arc<dyn ProbeRtt + Send + Sync>>,
            None,
            Some(1.5),
            CancellationToken::new(),
            &mut probes,
        )
        .unwrap();
        selector.chains[0].set_gauges_for_test(Some(Duration::from_millis(10)), Some(0.));
        selector.chains[0].set_probe_state_for_test(ProbeTaskState::Cancelled);
        selector.chains[1].set_gauges_for_test(Some(Duration::from_millis(5000)), Some(0.));
        for _ in 0..200 {
            assert_eq!(
                selector.choose_chain().weight,
                1,
                "a cancelled probe is healthy; its (better) gauges should win"
            );
        }
    }
}
