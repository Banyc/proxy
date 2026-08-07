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
    ConnConfig, GaugedConnChain, PROBE_ROUND_INTERVAL, ProbeRtt, WeightedConnChain,
    WeightedConnChainBuildError, WeightedConnChainBuilder,
    chain_selection::{EligibilityGate, ScoredChain, chain_score, pick_weighted},
};

/// The merged-config registries plus runtime handles the route builders resolve
/// their names against.
#[derive(Clone)]
pub struct Registries<'caller> {
    pub conn: &'caller HashMap<Arc<str>, ConnConfig>,
    pub matcher: &'caller Arc<HashMap<Arc<str>, crate::matcher::Matcher>>,
    pub conn_selector: &'caller HashMap<Arc<str>, ConnSelector>,
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
pub struct ConnSelectorBuilder {
    pub chains: Vec<WeightedConnChainBuilder>,
    #[serde(default, alias = "trace_rtt")]
    pub probe_rtt: bool,
    pub active_chains: Option<NonZeroUsize>,
    #[serde(default)]
    pub max_rtt_ratio: Option<f64>,
}
impl ConnSelectorBuilder {
    pub fn resolve(
        self,
        registries: &Registries<'_>,
        generation: &mut tokio::task::JoinSet<()>,
    ) -> Result<ConnSelector, ConnSelectorBuildError> {
        let chains = self
            .chains
            .into_iter()
            .map(|c| c.resolve(registries))
            .collect::<Result<_, _>>()
            .map_err(ConnSelectorBuildError::ChainConfig)?;
        let tracer = match self.probe_rtt {
            true => Some(registries.tracer.clone()),
            false => None,
        };
        ConnSelector::new(
            chains,
            tracer,
            self.active_chains,
            self.max_rtt_ratio,
            registries.cancellation.clone(),
            generation,
        )
        .map_err(Into::into)
    }
}
#[derive(Debug, Error)]
pub enum ConnSelectorBuildError {
    #[error("Chain config is invalid: {0}")]
    ChainConfig(#[source] WeightedConnChainBuildError),
    #[error("{0}")]
    ConnSelector(#[from] ConnSelectorError),
}

#[derive(Debug, Clone)]
pub enum ConnSelector {
    Empty,
    Some(NonEmptyConnSelector),
}
impl ConnSelector {
    /// Build the selector.
    ///
    /// Probe tasks are spawned directly into the caller-owned, generation
    /// `JoinSet` (`generation`), which is drained at the server boundary with
    /// `result.unwrap()` so probe panics propagate. Dropping the generation
    /// set aborts the probe tasks.
    pub fn new(
        chains: Vec<WeightedConnChain>,
        tracer: Option<Arc<dyn ProbeRtt + Send + Sync>>,
        active_chains: Option<NonZeroUsize>,
        max_rtt_ratio: Option<f64>,
        cancellation: CancellationToken,
        generation: &mut tokio::task::JoinSet<()>,
    ) -> Result<Self, ConnSelectorError> {
        if chains.is_empty() {
            return Ok(Self::Empty);
        }
        let selector = NonEmptyConnSelector::new(
            chains,
            tracer,
            active_chains,
            max_rtt_ratio,
            cancellation,
            generation,
        )?;
        Ok(Self::Some(selector))
    }
}

#[derive(Debug, Clone)]
pub struct NonEmptyConnSelector {
    chains: Arc<[GaugedConnChain]>,
    cum_weight: NonZeroUsize,
    score_store: Arc<RwLock<ScoreStore>>,
    active_chains: NonZeroUsize,
    gate: Option<EligibilityGate>,
}
impl NonEmptyConnSelector {
    /// Build the selector.
    ///
    /// Each chain's probe supervision task is spawned directly into the
    /// caller-owned, generation `JoinSet` (`generation`), so the caller only
    /// needs to reap that single set.
    pub fn new(
        chains: Vec<WeightedConnChain>,
        tracer: Option<Arc<dyn ProbeRtt + Send + Sync>>,
        active_chains: Option<NonZeroUsize>,
        max_rtt_ratio: Option<f64>,
        cancellation: CancellationToken,
        generation: &mut tokio::task::JoinSet<()>,
    ) -> Result<Self, ConnSelectorError> {
        let cum_weight = chains.iter().map(|c| c.weight).sum();
        if cum_weight == 0 {
            return Err(ConnSelectorError::ZeroAccumulatedWeight);
        }
        let cum_weight = NonZeroUsize::new(cum_weight).unwrap();
        let gate = match max_rtt_ratio {
            Some(r) if r.is_finite() && r >= 1.0 => Some(EligibilityGate::new(r)),
            Some(r) => return Err(ConnSelectorError::InvalidMaxRttRatio(r)),
            None => None,
        };

        let active_chains = match active_chains {
            Some(active_chains) => {
                if active_chains.get() > chains.len() {
                    return Err(ConnSelectorError::TooManyActiveChains);
                }
                active_chains
            }
            None => NonZeroUsize::new(chains.len()).unwrap(),
        };

        let chains = chains
            .into_iter()
            .map(|c| GaugedConnChain::new(c, tracer.clone(), cancellation.clone(), generation))
            .collect::<Arc<[_]>>();
        let score_store = Arc::new(RwLock::new(ScoreStore::new(None, PROBE_ROUND_INTERVAL)));
        Ok(Self {
            chains,
            cum_weight,
            score_store,
            active_chains,
            gate,
        })
    }

    pub fn choose_chain(&self) -> &WeightedConnChain {
        if self.chains.len() == 1 {
            return self.chains[0].weighted();
        }
        let scores = self.score_store.read().unwrap().get().cloned();
        let scores = match scores {
            Some(scores) => scores,
            None => {
                let scores: Arc<[_]> = self.scores().into();
                info!(?scores, "Calculated scores");
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
pub enum ConnSelectorError {
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
    use super::super::conn_chain::ProbeTaskState;
    use super::*;
    use std::time::Duration;
    fn chain(weight: usize) -> WeightedConnChain {
        WeightedConnChain {
            weight,
            chain: Arc::from(Vec::<ConnConfig>::new()),
            payload_crypto: None,
        }
    }
    #[test]
    fn a_zero_sum_falls_back_within_the_eligible_set() {
        let mut generation = tokio::task::JoinSet::new();
        let selector = NonEmptyConnSelector::new(
            vec![chain(0), chain(5)],
            None::<Arc<dyn ProbeRtt + Send + Sync>>,
            None,
            Some(1.5),
            CancellationToken::new(),
            &mut generation,
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
        let mut generation = tokio::task::JoinSet::new();
        let selector = NonEmptyConnSelector::new(
            vec![chain(1), chain(2)],
            None::<Arc<dyn ProbeRtt + Send + Sync>>,
            None,
            Some(1.5),
            CancellationToken::new(),
            &mut generation,
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
        let mut generation = tokio::task::JoinSet::new();
        let selector = NonEmptyConnSelector::new(
            vec![chain(1), chain(2)],
            None::<Arc<dyn ProbeRtt + Send + Sync>>,
            None,
            None,
            CancellationToken::new(),
            &mut generation,
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
        let mut generation = tokio::task::JoinSet::new();
        let selector = NonEmptyConnSelector::new(
            vec![chain(1), chain(2)],
            None::<Arc<dyn ProbeRtt + Send + Sync>>,
            None,
            Some(1.5),
            CancellationToken::new(),
            &mut generation,
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
