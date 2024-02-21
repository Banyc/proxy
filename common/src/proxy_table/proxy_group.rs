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

use crate::cache_cell::CacheCell;

use super::{
    AddressString, GaugedProxyChain, ProxyConfig, Tracer, WeightedProxyChain,
    WeightedProxyChainBuildError, WeightedProxyChainBuilder, TRACE_INTERVAL,
};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProxyGroupBuilder<AS> {
    pub chains: Vec<WeightedProxyChainBuilder<AS>>,
    pub trace_rtt: bool,
    pub active_chains: Option<NonZeroUsize>,
}
impl<AS> ProxyGroupBuilder<AS> {
    pub fn build<A, TB, T>(
        self,
        cx: ProxyGroupBuildContext<'_, A, TB>,
    ) -> Result<ProxyGroup<A>, ProxyGroupBuildError>
    where
        A: std::fmt::Debug + fmt::Display + Clone + Send + Sync + 'static,
        AS: AddressString<Address = A>,
        TB: TracerBuilder<Tracer = T>,
        T: Tracer<Address = A> + Sync + Send + 'static,
    {
        let chains = self
            .chains
            .into_iter()
            .map(|c| c.build(cx.proxy_server))
            .collect::<Result<_, _>>()
            .map_err(ProxyGroupBuildError::ChainConfig)?;
        let tracer = match self.trace_rtt {
            true => Some(cx.tracer_builder.build()),
            false => None,
        };
        Ok(ProxyGroup::new(
            chains,
            tracer,
            self.active_chains,
            cx.cancellation,
        )?)
    }
}
#[derive(Debug, Error)]
pub enum ProxyGroupBuildError {
    #[error("Chain config is invalid: {0}")]
    ChainConfig(#[source] WeightedProxyChainBuildError),
    #[error("{0}")]
    ProxyGroup(#[from] ProxyGroupError),
}
#[derive(Debug)]
pub struct ProxyGroupBuildContext<'caller, A, TB> {
    pub proxy_server: &'caller HashMap<Arc<str>, ProxyConfig<A>>,
    pub tracer_builder: &'caller TB,
    pub cancellation: CancellationToken,
}
impl<'caller, A, TB> Clone for ProxyGroupBuildContext<'caller, A, TB> {
    fn clone(&self) -> Self {
        Self {
            proxy_server: self.proxy_server,
            tracer_builder: self.tracer_builder,
            cancellation: self.cancellation.clone(),
        }
    }
}

pub trait TracerBuilder {
    type Tracer: Tracer + Send + Sync + 'static;
    fn build(&self) -> Self::Tracer;
}

#[derive(Debug, Clone)]
pub struct ProxyGroup<A> {
    chains: Arc<[GaugedProxyChain<A>]>,
    cum_weight: NonZeroUsize,
    score_store: Arc<RwLock<ScoreStore>>,
    active_chains: NonZeroUsize,
}
impl<A> ProxyGroup<A>
where
    A: std::fmt::Debug + fmt::Display + Clone + Send + Sync + 'static,
{
    pub fn new<T>(
        chains: Vec<WeightedProxyChain<A>>,
        tracer: Option<T>,
        active_chains: Option<NonZeroUsize>,
        cancellation: CancellationToken,
    ) -> Result<Self, ProxyGroupError>
    where
        T: Tracer<Address = A> + Send + Sync + 'static,
    {
        let cum_weight = chains.iter().map(|c| c.weight).sum();
        if cum_weight == 0 {
            return Err(ProxyGroupError::ZeroAccumulatedWeight);
        }
        let cum_weight = NonZeroUsize::new(cum_weight).unwrap();

        let active_chains = match active_chains {
            Some(active_chains) => {
                if active_chains.get() > chains.len() {
                    return Err(ProxyGroupError::TooManyActiveChains);
                }
                active_chains
            }
            None => NonZeroUsize::new(chains.len()).unwrap(),
        };

        let tracer = tracer.map(Arc::new);
        let chains = chains
            .into_iter()
            .map(|c| GaugedProxyChain::new(c, tracer.clone(), cancellation.clone()))
            .collect::<Arc<[_]>>();
        let score_store = Arc::new(RwLock::new(ScoreStore::new(None, TRACE_INTERVAL)));
        Ok(Self {
            chains,
            cum_weight,
            score_store,
            active_chains,
        })
    }

    pub fn choose_chain(&self) -> &WeightedProxyChain<A> {
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

        let mut rng = rand::thread_rng();
        if scores.sum == 0. {
            let i = rng.gen_range(0..self.chains.len());
            return self.chains[i].weighted();
        }
        let mut rand_score = rng.gen_range(0. ..scores.sum);
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
pub enum ProxyGroupError {
    #[error("Zero accumulated weight with chains")]
    ZeroAccumulatedWeight,
    #[error("The number of active chains is more than the number of chains")]
    TooManyActiveChains,
}

type ScoreStore = CacheCell<Scores>;

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
    hat
}
