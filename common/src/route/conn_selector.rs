use std::{
    collections::HashMap,
    fmt,
    num::NonZeroUsize,
    sync::{Arc, RwLock},
};

use rand::Rng;
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio_util::sync::CancellationToken;
use tracing::info;

use crate::ttl_cell::TtlCell;

use super::{
    ConnConfig, GaugedConnChain, IntoAddr, TRACE_INTERVAL, TraceRtt, WeightedConnChain,
    WeightedConnChainBuildError, WeightedConnChainBuilder,
};

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
        let mut rand_score = rng.random_range(0. ..scores.sum);
        for &(i, score) in scores.scores.iter() {
            if rand_score < score {
                return self.chains[i].weighted();
            }
            rand_score -= score;
        }
        unreachable!();
    }

    fn scores(&self) -> Vec<(usize, f64)> {
        let weights_hat = self
            .chains
            .iter()
            .map(|c| c.weighted().weight as f64 / self.cum_weight.get() as f64)
            .collect::<Vec<_>>();

        let rtt = self
            .chains
            .iter()
            .map(|c| c.rtt().map(|r| r.as_secs_f64()))
            .collect::<Vec<_>>();
        let rtt_hat = normalize(&rtt);

        let losses = self.chains.iter().map(|c| c.loss()).collect::<Vec<_>>();
        let losses_hat = normalize(&losses);

        let mut scores = (0..self.chains.len())
            .map(|i| (1. - losses_hat[i]).powi(3) * (1. - rtt_hat[i]).powi(2) * weights_hat[i])
            .enumerate()
            .collect::<Vec<_>>();

        scores.sort_by(|(_, a), (_, b)| b.partial_cmp(a).unwrap());
        scores[..self.active_chains.get()].to_vec()
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

fn normalize(list: &[Option<f64>]) -> Vec<f64> {
    let sum_some: f64 = list.iter().map(|x| x.unwrap_or(0.)).sum();
    let count_some = list.iter().map(|x| x.map(|_| 1).unwrap_or(0)).sum();
    let hat = match count_some {
        0 => {
            let hat_mean = 1. / list.len() as f64;
            (0..list.len()).map(|_| hat_mean).collect::<Vec<_>>()
        }
        _ => {
            let mean = sum_some / count_some as f64;
            let sum: f64 = list.iter().map(|x| x.unwrap_or(mean)).sum();
            if sum == 0. {
                (0..list.len()).map(|_| 0.).collect::<Vec<_>>()
            } else {
                let hat_mean = mean / sum;
                list.iter()
                    .map(|x| x.map(|x| x / sum).unwrap_or(hat_mean))
                    .collect::<Vec<_>>()
            }
        }
    };
    #[allow(clippy::let_and_return)]
    hat
}
