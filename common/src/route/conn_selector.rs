use std::{
    collections::HashMap,
    fmt,
    num::NonZeroUsize,
    sync::{Arc, RwLock},
    time::Duration,
};

use rand::RngExt;
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio_util::sync::CancellationToken;
use tracing::info;

use crate::ttl_cell::TtlCell;

use super::{
    ConnConfig, GaugedConnChain, IntoAddr, TraceRtt, WeightedConnChain,
    WeightedConnChainBuildError, WeightedConnChainBuilder, TRACE_INTERVAL,
};

const RTT_REF: Duration = Duration::from_millis(100);
const LOSS_EXP: i32 = 3;
const RTT_EXP: i32 = 1;
fn chain_score(weight: f64, loss: Option<f64>, rtt: Option<Duration>) -> f64 {
    let r0 = RTT_REF.as_secs_f64();
    let loss = loss.unwrap_or(0.).clamp(0., 1.);
    let rtt = rtt.map(|r| r.as_secs_f64()).unwrap_or(r0);
    let loss_factor = (1. - loss).powi(LOSS_EXP);
    let rtt_factor = (1. / (1. + rtt / r0)).powi(RTT_EXP);
    weight * loss_factor * rtt_factor
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConnSelectorBuilder<AddrStr> {
    pub chains: Vec<WeightedConnChainBuilder<AddrStr>>,
    pub trace_rtt: bool,
    pub active_chains: Option<NonZeroUsize>,
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
}
impl<Addr> ConnSelector1<Addr>
where
    Addr: std::fmt::Debug + fmt::Display + Clone + Send + Sync + 'static,
{
    pub fn new<T>(
        chains: Vec<WeightedConnChain<Addr>>,
        tracer: Option<T>,
        active_chains: Option<NonZeroUsize>,
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
            let i = rng.random_range(0..self.chains.len());
            return self.chains[i].weighted();
        }
        let r = rng.random_range(0. ..scores.sum);
        let i = pick_weighted(&scores.scores, r);
        self.chains[i].weighted()
    }

    fn scores(&self) -> Vec<(usize, f64)> {
        let cum_weight = self.cum_weight.get() as f64;
        let mut scores = self
            .chains
            .iter()
            .enumerate()
            .map(|(i, c)| {
                let weight = c.weighted().weight as f64 / cum_weight;
                (i, chain_score(weight, c.loss(), c.rtt()))
            })
            .collect::<Vec<_>>();
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
}

type ScoreStore = TtlCell<Scores>;
#[derive(Debug, Clone)]
struct Scores {
    scores: Arc<[(usize, f64)]>,
    sum: f64,
}

fn pick_weighted(scores: &[(usize, f64)], mut r: f64) -> usize {
    for &(i, score) in scores {
        if r < score {
            return i;
        }
        r -= score;
    }
    scores
        .last()
        .map(|&(i, _)| i)
        .expect("pick_weighted called with no scores")
}

#[cfg(test)]
mod tests {
    use super::*;
    fn ms(n: u64) -> Option<Duration> {
        Some(Duration::from_millis(n))
    }
    #[test]
    fn lower_loss_scores_higher() {
        assert!(chain_score(1., Some(0.0), ms(50)) > chain_score(1., Some(0.10), ms(50)));
    }
    #[test]
    fn lower_rtt_scores_higher() {
        assert!(chain_score(1., Some(0.0), ms(10)) > chain_score(1., Some(0.0), ms(300)));
    }
    #[test]
    fn a_lossy_chain_is_de_preferred_but_never_excluded() {
        let lossy = chain_score(1., Some(0.10), ms(50));
        assert!(lossy > 0.);
        assert!(lossy > chain_score(1., Some(0.0), ms(50)) * 0.5);
    }
    #[test]
    fn latency_factor_is_one_half_at_the_reference() {
        assert!((chain_score(1., Some(0.0), Some(RTT_REF)) - 0.5).abs() < 1e-9);
    }
    #[test]
    fn small_latency_differences_barely_matter() {
        let a = chain_score(1., Some(0.0), ms(10));
        let b = chain_score(1., Some(0.0), ms(20));
        assert!(b / a > 0.85, "ratio was {}", b / a);
    }
    #[test]
    fn unknown_metrics_are_neutral() {
        assert!((chain_score(1., None, None) - 0.5).abs() < 1e-9);
    }
    #[test]
    fn pick_weighted_selects_the_bucket_containing_r() {
        let s = [(0usize, 0.7_f64), (1, 0.3)];
        assert_eq!(pick_weighted(&s, 0.0), 0);
        assert_eq!(pick_weighted(&s, 0.69), 0);
        assert_eq!(pick_weighted(&s, 0.70), 1);
        assert_eq!(pick_weighted(&s, 0.99), 1);
    }
    #[test]
    fn pick_weighted_returns_the_chain_index_not_the_position() {
        let s = [(3usize, 0.5_f64), (1, 0.5)];
        assert_eq!(pick_weighted(&s, 0.2), 3);
        assert_eq!(pick_weighted(&s, 0.7), 1);
    }
    #[test]
    fn pick_weighted_single_bucket_always_wins() {
        assert_eq!(pick_weighted(&[(5usize, 1.0_f64)], 0.0), 5);
        assert_eq!(pick_weighted(&[(5usize, 1.0_f64)], 0.999), 5);
    }
    #[test]
    fn pick_weighted_overshoot_falls_back_to_last_instead_of_panicking() {
        assert_eq!(pick_weighted(&[(0usize, 0.3_f64), (1, 0.3)], 0.6), 1);
    }
}
