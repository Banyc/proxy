use std::{
    sync::{Arc, RwLock},
    time::Duration,
};

use async_trait::async_trait;
use rand::Rng;
use serde::{Deserialize, Serialize};
use tracing::{info, warn};

use crate::{crypto::XorCrypto, error::AnyError, header::route::RouteRequest};

const TRACE_INTERVAL: Duration = Duration::from_secs(30);

#[derive(Debug, Clone, PartialEq, Eq, Hash, Deserialize, Serialize)]
pub struct ProxyConfig<A> {
    pub address: A,
    pub crypto: XorCrypto,
}

pub fn convert_proxies_to_header_crypto_pairs<A>(
    nodes: &[ProxyConfig<A>],
    destination: Option<A>,
) -> Arc<[(RouteRequest<A>, &XorCrypto)]>
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
    pairs.into()
}

#[derive(Debug)]
pub struct ProxyTable<A> {
    chains: Arc<[GaugedProxyChain<A>]>,
    cum_weight: usize,
}

impl<A> ProxyTable<A>
where
    A: std::fmt::Debug + Clone + Send + Sync + 'static,
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
        Some(Self { chains, cum_weight })
    }

    pub fn choose_chain(&self) -> &WeightedProxyChain<A> {
        if self.chains.len() == 1 {
            return self.chains[0].weighted();
        }
        let mut rng = rand::thread_rng();
        let mut weight = rng.gen_range(0..self.cum_weight);
        for chain in self.chains.as_ref() {
            if weight < chain.weighted().weight {
                return chain.weighted();
            }
            weight -= chain.weighted().weight;
        }
        unreachable!();
    }
}

#[derive(Debug)]
pub struct WeightedProxyChain<A> {
    pub weight: usize,
    pub chain: Arc<[ProxyConfig<A>]>,
    pub payload_crypto: Option<XorCrypto>,
}

#[derive(Debug)]
struct GaugedProxyChain<A> {
    weighted: WeightedProxyChain<A>,
    rtt: Arc<RwLock<Option<Duration>>>,
    task_handle: Option<tokio::task::JoinHandle<()>>,
}

impl<A> GaugedProxyChain<A>
where
    A: std::fmt::Debug + Clone + Send + Sync + 'static,
{
    pub fn new<T>(weighted: WeightedProxyChain<A>, tracer: Option<Arc<T>>) -> Self
    where
        T: Tracer<Address = A> + Send + Sync + 'static,
    {
        let chain = weighted.chain.clone();
        let rtt = Arc::new(RwLock::new(None));
        let rtt_clone = rtt.clone();
        let task_handle = tracer.map(|tracer| {
            tokio::task::spawn(async move {
                loop {
                    let rtt = tracer.trace_rtt(&chain).await;
                    match rtt {
                        Ok(rtt) => {
                            let addresses =
                                chain.iter().map(|c| c.address.clone()).collect::<Vec<_>>();
                            info!(?addresses, ?rtt, "Traced RTT");
                            let mut rtt_clone = rtt_clone.write().unwrap();
                            *rtt_clone = Some(rtt);
                        }
                        Err(e) => warn!("{:?}", e),
                    }
                    tokio::time::sleep(TRACE_INTERVAL).await;
                }
            })
        });
        Self {
            weighted,
            rtt,
            task_handle,
        }
    }

    pub fn weighted(&self) -> &WeightedProxyChain<A> {
        &self.weighted
    }

    pub fn rtt(&self) -> Option<Duration> {
        *self.rtt.read().unwrap()
    }
}

impl<A> Drop for GaugedProxyChain<A> {
    fn drop(&mut self) {
        if let Some(h) = self.task_handle.as_ref() {
            h.abort()
        }
    }
}

#[async_trait]
pub trait Tracer {
    type Address;
    async fn trace_rtt(&self, chain: &[ProxyConfig<Self::Address>]) -> Result<Duration, AnyError>;
}
