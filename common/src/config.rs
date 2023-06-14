use rand::Rng;
use serde::{Deserialize, Serialize};

use crate::crypto::XorCrypto;

#[derive(Debug, Clone, PartialEq, Eq, Hash, Deserialize, Serialize)]
pub struct ProxyConfig<A> {
    pub address: A,
    pub crypto: XorCrypto,
}

pub fn convert_proxies_to_header_crypto_pairs<A>(
    nodes: &[ProxyConfig<A>],
    destination: A,
) -> Vec<(A, &XorCrypto)>
where
    A: Clone,
{
    let mut pairs = Vec::new();
    for i in 0..nodes.len() - 1 {
        let node = &nodes[i];
        let next_node = &nodes[i + 1];
        pairs.push((next_node.address.clone(), &node.crypto));
    }
    pairs.push((destination, &nodes.last().unwrap().crypto));
    pairs
}

pub struct ProxyTable<A> {
    chains: Vec<WeightedProxyChain<A>>,
    cum_weight: usize,
}

impl<A> ProxyTable<A> {
    pub fn new(chains: Vec<WeightedProxyChain<A>>) -> Option<Self> {
        let cum_weight = chains.iter().map(|c| c.weight).sum();
        if cum_weight == 0 {
            return None;
        }
        Some(Self { chains, cum_weight })
    }

    pub fn add_chain(&mut self, chain: WeightedProxyChain<A>) {
        self.cum_weight += chain.weight;
        self.chains.push(chain);
    }

    pub fn choose_chain(&self) -> &WeightedProxyChain<A> {
        if self.chains.len() == 1 {
            return &self.chains[0];
        }
        let mut rng = rand::thread_rng();
        let mut weight = rng.gen_range(0..self.cum_weight);
        for chain in &self.chains {
            if weight < chain.weight {
                return chain;
            }
            weight -= chain.weight;
        }
        unreachable!();
    }
}

pub struct WeightedProxyChain<A> {
    pub weight: usize,
    pub chain: Vec<ProxyConfig<A>>,
}
