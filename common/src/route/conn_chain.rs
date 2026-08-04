use std::{
    collections::HashMap,
    fmt,
    sync::{Arc, RwLock},
    time::Duration,
};

use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio_util::sync::CancellationToken;

use crate::{config::SharableConfig, header::route::RouteRequest};

use super::{
    ConnConfig, ConnConfigBuildError, ConnConfigBuilder, IntoAddr, TraceRtt, prober::spawn_tracer,
    rtt_stats::RttStats,
};

pub const TRACE_INTERVAL: Duration = Duration::from_secs(30);

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
    rtt_stats: Arc<RwLock<RttStats>>,
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

    pub fn weighted(&self) -> &WeightedConnChain<Addr> {
        &self.weighted
    }

    pub fn rtt(&self) -> Option<Duration> {
        self.rtt_stats.read().unwrap().srtt
    }

    pub fn rttvar(&self) -> Option<Duration> {
        self.rtt_stats.read().unwrap().rttvar
    }

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
impl<Addr> Drop for GaugedConnChain<Addr> {
    fn drop(&mut self) {
        if let Some(h) = self.task_handle.as_ref() {
            h.abort()
        }
    }
}
