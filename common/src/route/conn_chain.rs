use std::{
    collections::HashMap,
    fmt,
    sync::{Arc, RwLock},
    time::Duration,
};

use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio_util::sync::CancellationToken;
use tracing::{info, trace};

use crate::{config::SharableConfig, error::AnyError, header::route::RouteRequest};

use super::{ConnConfig, ConnConfigBuildError, ConnConfigBuilder, IntoAddr};

pub const TRACE_INTERVAL: Duration = Duration::from_secs(30);
const TRACE_DEAD_INTERVAL: Duration = Duration::from_secs(60 * 2);
const TRACES_PER_WAVE: usize = 60;
const TRACE_BURST_GAP: Duration = Duration::from_millis(10);
const RTT_TIMEOUT: Duration = Duration::from_secs(60);

pub type ConnChain<Addr> = [ConnConfig<Addr>];

/// # Panic
///
/// `nodes` must not be empty.
pub fn convert_proxies_to_header_crypto_pairs<Addr>(
    nodes: &ConnChain<Addr>,
    destination: Option<Addr>,
) -> Vec<(RouteRequest<Addr>, &tokio_chacha20::config::Config)>
where
    Addr: Clone + Sync + Send,
{
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
pub struct WeightedConnChainBuilder<AddrStr> {
    pub weight: usize,
    pub chain: Vec<SharableConfig<ConnConfigBuilder<AddrStr>>>,
}
impl<AddrStr> WeightedConnChainBuilder<AddrStr> {
    pub fn build<Addr: Clone>(
        self,
        conn: &HashMap<Arc<str>, ConnConfig<Addr>>,
    ) -> Result<WeightedConnChain<Addr>, WeightedConnChainBuildError>
    where
        AddrStr: IntoAddr<Addr = Addr>,
    {
        let chain = self
            .chain
            .into_iter()
            .map(|c| match c {
                SharableConfig::SharingKey(k) => conn
                    .get(&k)
                    .cloned()
                    .ok_or(WeightedConnChainBuildError::ProxyServerKeyNotFound(k)),
                SharableConfig::Private(c) => c.build().map_err(Into::into),
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
pub struct WeightedConnChain<Addr> {
    pub weight: usize,
    pub chain: Arc<ConnChain<Addr>>,
    pub payload_crypto: Option<tokio_chacha20::config::Config>,
}

#[derive(Debug)]
pub struct GaugedConnChain<Addr> {
    weighted: WeightedConnChain<Addr>,
    rtt: Arc<RwLock<Option<Duration>>>,
    loss: Arc<RwLock<Option<f64>>>,
    task_handle: Option<tokio::task::JoinHandle<()>>,
}
impl<Addr> GaugedConnChain<Addr>
where
    Addr: std::fmt::Debug + fmt::Display + Clone + Send + Sync + 'static,
{
    pub fn new<T>(
        weighted: WeightedConnChain<Addr>,
        tracer: Option<Arc<T>>,
        cancellation: CancellationToken,
    ) -> Self
    where
        T: TraceRtt<Addr = Addr> + Send + Sync + 'static,
    {
        let rtt = Arc::new(RwLock::new(None));
        let loss = Arc::new(RwLock::new(None));
        let task_handle = tracer.map(|tracer| {
            spawn_tracer(
                tracer,
                weighted.chain.clone(),
                rtt.clone(),
                loss.clone(),
                cancellation,
            )
        });
        Self {
            weighted,
            rtt,
            loss,
            task_handle,
        }
    }

    pub fn weighted(&self) -> &WeightedConnChain<Addr> {
        &self.weighted
    }

    pub fn rtt(&self) -> Option<Duration> {
        *self.rtt.read().unwrap()
    }

    pub fn loss(&self) -> Option<f64> {
        *self.loss.read().unwrap()
    }
}
impl<Addr> Drop for GaugedConnChain<Addr> {
    fn drop(&mut self) {
        if let Some(h) = self.task_handle.as_ref() {
            h.abort()
        }
    }
}

fn spawn_tracer<Tracer, Addr>(
    tracer: Arc<Tracer>,
    chain: Arc<ConnChain<Addr>>,
    rtt_store: Arc<RwLock<Option<Duration>>>,
    loss_store: Arc<RwLock<Option<f64>>>,
    cancellation: CancellationToken,
) -> tokio::task::JoinHandle<()>
where
    Tracer: TraceRtt<Addr = Addr> + Send + Sync + 'static,
    Addr: fmt::Display + Send + Sync + 'static,
{
    tokio::task::spawn(async move {
        let mut wave = tokio::task::JoinSet::new();
        while !cancellation.is_cancelled() {
            // Spawn tracing tasks
            for _ in 0..TRACES_PER_WAVE {
                let chain = chain.clone();
                let tracer = tracer.clone();
                wave.spawn(async move {
                    tokio::time::timeout(RTT_TIMEOUT, tracer.trace_rtt(&chain)).await
                });
                tokio::time::sleep(TRACE_BURST_GAP).await;
            }

            // Collect RTT
            let mut rtt_sum = Duration::from_secs(0);
            let mut rtt_count: usize = 0;
            while let Some(res) = wave.join_next().await {
                let res = match res.unwrap() {
                    Ok(res) => res,
                    Err(_) => {
                        trace!("Trace timeout");
                        continue;
                    }
                };
                match res {
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

pub struct DisplayChain<'chain, Addr>(&'chain ConnChain<Addr>);
impl<Addr> fmt::Display for DisplayChain<'_, Addr>
where
    Addr: fmt::Display,
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

pub trait TraceRtt {
    type Addr;
    fn trace_rtt(
        &self,
        chain: &ConnChain<Self::Addr>,
    ) -> impl Future<Output = Result<Duration, AnyError>> + Send;
}
