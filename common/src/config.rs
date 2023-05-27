use serde::{Deserialize, Serialize};

use crate::crypto::XorCrypto;

#[derive(Debug, Clone, PartialEq, Eq, Hash, Deserialize, Serialize)]
pub struct ProxyConfig<A> {
    pub address: A,
    pub crypto: XorCrypto,
}

pub fn convert_proxy_configs_to_header_crypto_pairs<A>(
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
