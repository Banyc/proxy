use std::{sync::Arc, time::Duration};

use async_trait::async_trait;
use rand::Rng;
use serde::{Deserialize, Serialize};

use crate::{crypto::XorCrypto, error::AnyError, header::route::RouteRequest};

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
    chains: Arc<[WeightedProxyChain<A>]>,
    cum_weight: usize,
}

impl<A> ProxyTable<A> {
    pub fn new(chains: Arc<[WeightedProxyChain<A>]>) -> Option<Self> {
        let cum_weight = chains.iter().map(|c| c.weight).sum();
        if cum_weight == 0 {
            return None;
        }
        Some(Self { chains, cum_weight })
    }

    pub fn choose_chain(&self) -> &WeightedProxyChain<A> {
        if self.chains.len() == 1 {
            return &self.chains[0];
        }
        let mut rng = rand::thread_rng();
        let mut weight = rng.gen_range(0..self.cum_weight);
        for chain in self.chains.as_ref() {
            if weight < chain.weight {
                return chain;
            }
            weight -= chain.weight;
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

#[async_trait]
pub trait Tracer {
    type Address;
    async fn trace_rtt(&self, chain: &[ProxyConfig<Self::Address>]) -> Result<Duration, AnyError>;
}
