use std::{
    fmt::Display,
    sync::{Arc, RwLock},
    time::Duration,
};

use async_trait::async_trait;
use rand::Rng;
use serde::{Deserialize, Serialize};
use tracing::{info, trace};

use crate::{
    cache_cell::CacheCell, crypto::XorCrypto, error::AnyError, header::route::RouteRequest,
};

const TRACE_INTERVAL: Duration = Duration::from_secs(30);
const TRACE_DEAD_INTERVAL: Duration = Duration::from_secs(60 * 60);
const TRACES_PER_WAVE: usize = 60;
const TRACE_BURST_GAP: Duration = Duration::from_millis(10);

pub type ProxyChain<A> = [ProxyConfig<A>];

#[derive(Debug, Clone, PartialEq, Eq, Hash, Deserialize, Serialize)]
pub struct ProxyConfig<A> {
    pub address: A,
    pub crypto: XorCrypto,
}

/// # Panic
///
/// `nodes` must not be empty.
pub fn convert_proxies_to_header_crypto_pairs<A>(
    nodes: &ProxyChain<A>,
    destination: Option<A>,
) -> Vec<(RouteRequest<A>, &XorCrypto)>
where
    A: Clone,
{
    let mut pairs = Vec::new();
    for i in 0..nodes.len() - 1 {
        let node = &nodes[i];
        let next_node = &nodes[i + 1];
        let route_req = RouteRequest {
            upstream: Some(next_node.address.clone()),
        };
        pairs.push((route_req, &node.crypto));
    }
    let route_req = RouteRequest {
        upstream: destination,
    };
    pairs.push((route_req, &nodes.last().unwrap().crypto));
    pairs
}

#[derive(Debug, Clone)]
pub struct ProxyTable<A> {
    chains: Arc<[GaugedProxyChain<A>]>,
    cum_weight: usize,
    score_store: Arc<RwLock<ScoreStore>>,
}

impl<A> ProxyTable<A>
where
    A: std::fmt::Debug + Display + Clone + Send + Sync + 'static,
{
    pub fn new<T>(chains: Vec<WeightedProxyChain<A>>, tracer: Option<T>) -> Option<Self>
    where
        T: Tracer<Address = A> + Send + Sync + 'static,
    {
        let cum_weight = chains.iter().map(|c| c.weight).sum();
        if cum_weight == 0 {
            return None;
        }

        let tracer = tracer.map(Arc::new);
        let chains = chains
            .into_iter()
            .map(|c| GaugedProxyChain::new(c, tracer.clone()))
            .collect::<Arc<[_]>>();
        let score_store = Arc::new(RwLock::new(ScoreStore::new(None, TRACE_INTERVAL)));
        Some(Self {
            chains,
            cum_weight,
            score_store,
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
                let scores: Arc<[f64]> = self.scores().into();
                info!(?scores, "Calculated scores");
                self.score_store.write().unwrap().set(Arc::clone(&scores));
                scores
            }
        };

        let mut rng = rand::thread_rng();
        let sum_scores = scores.iter().sum::<f64>();
        if sum_scores == 0. {
            let i = rng.gen_range(0..self.chains.len());
            return self.chains[i].weighted();
        }
        let mut rand_score = rng.gen_range(0. ..sum_scores);
        for (score, chain) in scores.iter().zip(self.chains.iter()) {
            if rand_score < *score {
                return chain.weighted();
            }
            rand_score -= score;
        }
        unreachable!();
    }

    fn scores(&self) -> Vec<f64> {
        let weights_hat = self
            .chains
            .iter()
            .map(|c| c.weighted().weight as f64 / self.cum_weight as f64)
            .collect::<Vec<_>>();

        let rtt = self
            .chains
            .iter()
            .map(|c| c.rtt().map(|r| r.as_secs_f64()))
            .collect::<Vec<_>>();
        let rtt_hat = normalize(&rtt);

        let losses = self.chains.iter().map(|c| c.loss()).collect::<Vec<_>>();
        let losses_hat = normalize(&losses);

        (0..self.chains.len())
            .map(|i| (1. - losses_hat[i]).powi(3) * (1. - rtt_hat[i]).powi(2) * weights_hat[i])
            .collect::<Vec<_>>()
    }
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

type ScoreStore = CacheCell<Arc<[f64]>>;

#[derive(Debug)]
pub struct WeightedProxyChain<A> {
    pub weight: usize,
    pub chain: Arc<ProxyChain<A>>,
    pub payload_crypto: Option<XorCrypto>,
}

#[derive(Debug)]
struct GaugedProxyChain<A> {
    weighted: WeightedProxyChain<A>,
    rtt: Arc<RwLock<Option<Duration>>>,
    loss: Arc<RwLock<Option<f64>>>,
    task_handle: Option<tokio::task::JoinHandle<()>>,
}

impl<A> GaugedProxyChain<A>
where
    A: std::fmt::Debug + Display + Clone + Send + Sync + 'static,
{
    pub fn new<T>(weighted: WeightedProxyChain<A>, tracer: Option<Arc<T>>) -> Self
    where
        T: Tracer<Address = A> + Send + Sync + 'static,
    {
        let rtt = Arc::new(RwLock::new(None));
        let loss = Arc::new(RwLock::new(None));
        let task_handle = tracer
            .map(|tracer| spawn_tracer(tracer, weighted.chain.clone(), rtt.clone(), loss.clone()));
        Self {
            weighted,
            rtt,
            loss,
            task_handle,
        }
    }

    pub fn weighted(&self) -> &WeightedProxyChain<A> {
        &self.weighted
    }

    pub fn rtt(&self) -> Option<Duration> {
        *self.rtt.read().unwrap()
    }

    pub fn loss(&self) -> Option<f64> {
        *self.loss.read().unwrap()
    }
}

fn spawn_tracer<T, A>(
    tracer: Arc<T>,
    chain: Arc<ProxyChain<A>>,
    rtt_store: Arc<RwLock<Option<Duration>>>,
    loss_store: Arc<RwLock<Option<f64>>>,
) -> tokio::task::JoinHandle<()>
where
    T: Tracer<Address = A> + Send + Sync + 'static,
    A: Display + Send + Sync + 'static,
{
    tokio::task::spawn(async move {
        let mut wave = tokio::task::JoinSet::new();
        loop {
            // Spawn tracing tasks
            for _ in 0..TRACES_PER_WAVE {
                let chain = chain.clone();
                let tracer = tracer.clone();
                wave.spawn(async move { tracer.trace_rtt(&chain).await });
                tokio::time::sleep(TRACE_BURST_GAP).await;
            }

            // Collect RTT
            let mut rtt_sum = Duration::from_secs(0);
            let mut rtt_count: usize = 0;
            while let Some(res) = wave.join_next().await {
                match res.unwrap() {
                    Ok(rtt) => {
                        rtt_sum += rtt;
                        rtt_count += 1;
                    }
                    Err(e) => {
                        trace!("{:?}", e);
                    }
                }
            }
            let rtt = if rtt_count == 0 {
                None
            } else {
                Some(rtt_sum / (rtt_count as u32))
            };
            let loss = (TRACES_PER_WAVE - rtt_count) as f64 / TRACES_PER_WAVE as f64;

            // Store RTT
            let addresses = DisplayChain(&chain);
            info!(%addresses, ?rtt, ?loss, "Traced RTT");
            {
                let mut rtt_store = rtt_store.write().unwrap();
                *rtt_store = rtt;
            }
            {
                let mut loss_store = loss_store.write().unwrap();
                *loss_store = Some(loss);
            }

            // Sleep
            if rtt_count == 0 {
                tokio::time::sleep(TRACE_DEAD_INTERVAL).await;
            } else {
                tokio::time::sleep(TRACE_INTERVAL).await;
            }
        }
    })
}

impl<A> Drop for GaugedProxyChain<A> {
    fn drop(&mut self) {
        if let Some(h) = self.task_handle.as_ref() {
            h.abort()
        }
    }
}

pub struct DisplayChain<'chain, A>(&'chain ProxyChain<A>);

impl<'chain, A> Display for DisplayChain<'chain, A>
where
    A: Display,
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

#[async_trait]
pub trait Tracer {
    type Address;
    async fn trace_rtt(&self, chain: &ProxyChain<Self::Address>) -> Result<Duration, AnyError>;
}
