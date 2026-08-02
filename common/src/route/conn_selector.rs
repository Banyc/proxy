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

use crate::ttl_cell::TtlCell;

use super::{
    ConnConfig, GaugedConnChain, IntoAddr, TRACE_INTERVAL, TraceRtt, WeightedConnChain,
    WeightedConnChainBuildError, WeightedConnChainBuilder,
    chain_selection::{EligibilityGate, ScoredChain, chain_score, pick_weighted},
};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConnSelectorBuilder<AddrStr> {
    pub chains: Vec<WeightedConnChainBuilder<AddrStr>>,
    pub trace_rtt: bool,
    pub active_chains: Option<NonZeroUsize>,
    #[serde(default)]
    pub max_rtt_ratio: Option<f64>,
}
impl<AddrStr> ConnSelectorBuilder<AddrStr> {
    pub fn build<Addr, TracerBuilder, Tracer>(
        self,
        cx: ConnSelectorBuildContext<'_, Addr, TracerBuilder>,
    ) -> Result<ConnSelector<Addr>, ConnSelectorBuildError>
    where
        Addr: std::fmt::Debug + fmt::Display + Clone + Send + Sync + 'static,
        AddrStr: IntoAddr<Addr = Addr>,
        TracerBuilder: BuildTracer<Tracer = Tracer>,
        Tracer: TraceRtt<Addr = Addr> + Sync + Send + 'static,
    {
        let chains = self
            .chains
            .into_iter()
            .map(|c| c.build(cx.conn))
            .collect::<Result<_, _>>()
            .map_err(ConnSelectorBuildError::ChainConfig)?;
        let tracer = match self.trace_rtt {
            true => Some(cx.tracer_builder.build()),
            false => None,
        };
        Ok(ConnSelector::new(
            chains,
            tracer,
            self.active_chains,
            self.max_rtt_ratio,
            cx.cancellation,
        )?)
    }
}
#[derive(Debug, Error)]
pub enum ConnSelectorBuildError {
    #[error("Chain config is invalid: {0}")]
    ChainConfig(#[source] WeightedConnChainBuildError),
    #[error("{0}")]
    ConnSelector(#[from] ConnSelectorError),
}
#[derive(Debug)]
pub struct ConnSelectorBuildContext<'caller, Addr, TracerBuilder> {
    pub conn: &'caller HashMap<Arc<str>, ConnConfig<Addr>>,
    pub tracer_builder: &'caller TracerBuilder,
    pub cancellation: CancellationToken,
}
impl<Addr, TracerBuilder> Clone for ConnSelectorBuildContext<'_, Addr, TracerBuilder> {
    fn clone(&self) -> Self {
        Self {
            conn: self.conn,
            tracer_builder: self.tracer_builder,
            cancellation: self.cancellation.clone(),
        }
    }
}

pub trait BuildTracer {
    type Tracer: TraceRtt + Send + Sync + 'static;
    fn build(&self) -> Self::Tracer;
}

#[derive(Debug, Clone)]
pub enum ConnSelector<Addr> {
    Empty,
    Some(ConnSelector1<Addr>),
}
impl<Addr> ConnSelector<Addr>
where
    Addr: std::fmt::Debug + fmt::Display + Clone + Send + Sync + 'static,
{
    pub fn new<T>(
        chains: Vec<WeightedConnChain<Addr>>,
        tracer: Option<T>,
        active_chains: Option<NonZeroUsize>,
        max_rtt_ratio: Option<f64>,
        cancellation: CancellationToken,
    ) -> Result<Self, ConnSelectorError>
    where
        T: TraceRtt<Addr = Addr> + Send + Sync + 'static,
    {
        if chains.is_empty() {
            return Ok(Self::Empty);
        }
        Ok(Self::Some(ConnSelector1::new(
            chains,
            tracer,
            active_chains,
            max_rtt_ratio,
            cancellation,
        )?))
    }
}

#[derive(Debug, Clone)]
pub struct ConnSelector1<Addr> {
    chains: Arc<[GaugedConnChain<Addr>]>,
    cum_weight: NonZeroUsize,
    score_store: Arc<RwLock<ScoreStore>>,
    active_chains: NonZeroUsize,
    gate: Option<EligibilityGate>,
}
impl<Addr> ConnSelector1<Addr>
where
    Addr: std::fmt::Debug + fmt::Display + Clone + Send + Sync + 'static,
{
    pub fn new<T>(
        chains: Vec<WeightedConnChain<Addr>>,
        tracer: Option<T>,
        active_chains: Option<NonZeroUsize>,
        max_rtt_ratio: Option<f64>,
        cancellation: CancellationToken,
    ) -> Result<Self, ConnSelectorError>
    where
        T: TraceRtt<Addr = Addr> + Send + Sync + 'static,
    {
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

        let tracer = tracer.map(Arc::new);
        let chains = chains
            .into_iter()
            .map(|c| GaugedConnChain::new(c, tracer.clone(), cancellation.clone()))
            .collect::<Arc<[_]>>();
        let score_store = Arc::new(RwLock::new(ScoreStore::new(None, TRACE_INTERVAL)));
        Ok(Self {
            chains,
            cum_weight,
            score_store,
            active_chains,
            gate,
        })
    }

    pub fn choose_chain(&self) -> &WeightedConnChain<Addr> {
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
                let rtt = c.rtt_eff();
                let weight = c.weighted().weight as f64 / cum_weight;
                ScoredChain {
                    index,
                    score: chain_score(weight, c.loss(), rtt),
                    rtt,
                }
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
    use super::*;
    use crate::{error::AnyError, route::ConnChain};
    use std::net::SocketAddr;
    use std::time::Duration;
    struct NoTracer;
    impl TraceRtt for NoTracer {
        type Addr = SocketAddr;
        async fn trace_rtt(&self, _chain: &ConnChain<SocketAddr>) -> Result<Duration, AnyError> {
            unreachable!("the gauges are set directly")
        }
    }
    fn chain(weight: usize) -> WeightedConnChain<SocketAddr> {
        WeightedConnChain {
            weight,
            chain: Arc::from(Vec::<ConnConfig<SocketAddr>>::new()),
            payload_crypto: None,
        }
    }
    #[test]
    fn a_zero_sum_falls_back_within_the_eligible_set() {
        let selector = ConnSelector1::new(
            vec![chain(0), chain(5)],
            None::<NoTracer>,
            None,
            Some(1.5),
            CancellationToken::new(),
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
}
