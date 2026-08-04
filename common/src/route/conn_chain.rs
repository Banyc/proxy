use std::{
    sync::{Arc, RwLock},
    time::Duration,
};

use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio_util::sync::CancellationToken;

use crate::{config::SharableConfig, header::route::RouteRequest, proto::addr::RouteAddr};

use super::{
    ConnConfig, ConnConfigBuildError, ProbeRtt, Registries, prober::spawn_tracer,
    rtt_stats::RttStats,
};

pub const PROBE_ROUND_INTERVAL: Duration = Duration::from_secs(30);

pub type ConnChain = [ConnConfig];

/// # Panic
///
/// `nodes` must not be empty.
pub fn convert_proxies_to_header_crypto_pairs(
    nodes: &ConnChain,
    destination: Option<RouteAddr>,
) -> Vec<(RouteRequest<RouteAddr>, &tokio_chacha20::config::Config)> {
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
pub struct WeightedConnChainBuilder {
    pub weight: usize,
    pub chain: Vec<SharableConfig<ConnConfig>>,
}
impl WeightedConnChainBuilder {
    pub fn resolve(
        self,
        registries: &Registries<'_>,
    ) -> Result<WeightedConnChain, WeightedConnChainBuildError> {
        let chain = self
            .chain
            .into_iter()
            .map(|c| match c {
                SharableConfig::SharingKey(k) => registries
                    .conn
                    .get(&k)
                    .cloned()
                    .ok_or(WeightedConnChainBuildError::ProxyServerKeyNotFound(k)),
                SharableConfig::Private(c) => Ok(c),
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
pub struct WeightedConnChain {
    pub weight: usize,
    pub chain: Arc<ConnChain>,
    pub payload_crypto: Option<tokio_chacha20::config::Config>,
}

#[derive(Debug)]
pub struct GaugedConnChain {
    weighted: WeightedConnChain,
    rtt_stats: Arc<RwLock<RttStats>>,
    loss: Arc<RwLock<Option<f64>>>,
    task_handle: Option<tokio::task::JoinHandle<()>>,
}
impl GaugedConnChain {
    pub fn new(
        weighted: WeightedConnChain,
        tracer: Option<Arc<dyn ProbeRtt + Send + Sync>>,
        cancellation: CancellationToken,
    ) -> Self {
        let rtt_stats = Arc::new(RwLock::new(RttStats::default()));
        let loss = Arc::new(RwLock::new(None));
        let task_handle = tracer.map(|tracer| {
            spawn_tracer(
                tracer,
                weighted.chain.clone(),
                rtt_stats.clone(),
                loss.clone(),
                cancellation,
            )
        });
        Self {
            weighted,
            rtt_stats,
            loss,
            task_handle,
        }
    }

    pub fn weighted(&self) -> &WeightedConnChain {
        &self.weighted
    }

    pub fn rtt(&self) -> Option<Duration> {
        self.rtt_stats.read().unwrap().srtt
    }

    pub fn rttvar(&self) -> Option<Duration> {
        self.rtt_stats.read().unwrap().rttvar
    }

    /// `srtt + 2*rttvar` (the RFC 6298-style effective RTT used for chain scoring).
    pub fn rtt_eff(&self) -> Option<Duration> {
        self.rtt_stats.read().unwrap().effective()
    }

    pub fn loss(&self) -> Option<f64> {
        *self.loss.read().unwrap()
    }

    #[cfg(test)]
    pub(crate) fn set_gauges_for_test(&self, rtt_sample: Option<Duration>, loss: Option<f64>) {
        if let Some(rtt_sample) = rtt_sample {
            self.rtt_stats.write().unwrap().apply_sample(rtt_sample);
        }
        *self.loss.write().unwrap() = loss;
    }
}
impl Drop for GaugedConnChain {
    fn drop(&mut self) {
        if let Some(h) = self.task_handle.as_ref() {
            h.abort()
        }
    }
}
