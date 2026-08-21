//! Configuration types, the loader/prepare pipeline, and build errors for
//! reverse tunnels.

use std::{collections::HashSet, convert::Infallible, io, sync::Arc};

use common::{
    config::Merge,
    error::{AnyError, AnyResult},
    loading,
    proxy_runtime::{
        addr::{ReverseTunnelTransport, RouteAddr, RouteAddrStr},
        context::Runtime,
    },
};
use serde::Deserialize;
use thiserror::Error;
use tokio::task::JoinSet;

use super::initiator::{
    ReverseTunnelInitiatorBuilder, ReverseTunnelInitiatorHandler, initiator_transport,
};
use super::responder::{
    ReverseTunnelResponderHandler, RtpReverseTunnelResponderBuilder,
    TcpReverseTunnelResponderBuilder,
};

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct ReverseTunnelConfig {
    #[serde(default)]
    pub initiator: Vec<ReverseTunnelInitiatorConfig>,
    #[serde(default)]
    pub responder: Vec<ReverseTunnelResponderConfig>,
}
impl Merge for ReverseTunnelConfig {
    type Error = Infallible;
    fn merge(mut self, other: Self) -> Result<Self, Self::Error> {
        self.initiator.extend(other.initiator);
        self.responder.extend(other.responder);
        Ok(self)
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReverseTunnelInitiatorConfig {
    pub name: Arc<str>,
    pub responder_addr: RouteAddrStr,
    pub header_key: tokio_chacha20::config::ConfigBuilder,
    #[serde(default)]
    pub payload_key: Option<tokio_chacha20::config::ConfigBuilder>,
    #[serde(default)]
    pub allow_loopback: bool,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReverseTunnelResponderConfig {
    pub listen_addr: RouteAddrStr,
    pub header_key: tokio_chacha20::config::ConfigBuilder,
}

#[derive(Debug, Error)]
pub enum BuildError {
    #[error("invalid reverse tunnel name")]
    InvalidName,
    #[error("unsupported reverse tunnel transport `{0}`; expected `tcp` or `rtpmux`")]
    UnsupportedPhysicalTransport(Arc<str>),
    #[error("header crypto: {0}")]
    HeaderCrypto(String),
    #[error("payload crypto: {0}")]
    PayloadCrypto(String),
    #[error("failed to bind reverse tunnel responder: {0}")]
    Bind(#[from] io::Error),
    #[error("duplicate reverse tunnel configuration key `{0}`")]
    DuplicateKey(Arc<str>),
}

fn responder_transport(addr: &RouteAddr) -> Result<ReverseTunnelTransport, BuildError> {
    initiator_transport(addr)
}

#[derive(Debug)]
pub struct ReverseTunnelLoader {
    initiator: loading::Loader<ReverseTunnelInitiatorHandler>,
    tcp_responder: loading::Loader<ReverseTunnelResponderHandler>,
    rtp_responder: loading::Loader<ReverseTunnelResponderHandler>,
}
impl ReverseTunnelLoader {
    pub fn new() -> Self {
        Self {
            initiator: loading::Loader::new(),
            tcp_responder: loading::Loader::new(),
            rtp_responder: loading::Loader::new(),
        }
    }
    pub fn commit(
        &mut self,
        tasks: &mut JoinSet<AnyResult>,
        prepared: PreparedReverseTunnel,
    ) -> AnyResult {
        self.initiator.commit(tasks, prepared.initiator)?;
        self.tcp_responder.commit(tasks, prepared.tcp_responder)?;
        self.rtp_responder.commit(tasks, prepared.rtp_responder)?;
        Ok(())
    }

    /// A read-only snapshot of the live loaders, for preparation. The
    /// snapshot resolves against the same live listeners but cannot commit.
    pub fn snapshot(&self) -> ReverseTunnelLoaderSnapshot {
        ReverseTunnelLoaderSnapshot {
            initiator: self.initiator.snapshot(),
            tcp_responder: self.tcp_responder.snapshot(),
            rtp_responder: self.rtp_responder.snapshot(),
        }
    }
}
impl Default for ReverseTunnelLoader {
    fn default() -> Self {
        Self::new()
    }
}

/// An immutable snapshot of the live [`ReverseTunnelLoader`]s, taken by
/// [`ReverseTunnelLoader::snapshot`] for preparation. It can resolve and
/// bind builders against the live listener set, but it cannot commit —
/// replacement authority stays with the single owning loader.
pub struct ReverseTunnelLoaderSnapshot {
    initiator: loading::LoaderSnapshot<ReverseTunnelInitiatorHandler>,
    tcp_responder: loading::LoaderSnapshot<ReverseTunnelResponderHandler>,
    rtp_responder: loading::LoaderSnapshot<ReverseTunnelResponderHandler>,
}

pub struct PreparedReverseTunnel {
    initiator: loading::PreparedOps<ReverseTunnelInitiatorHandler>,
    tcp_responder: loading::PreparedOps<ReverseTunnelResponderHandler>,
    rtp_responder: loading::PreparedOps<ReverseTunnelResponderHandler>,
}

pub async fn prepare(
    config: ReverseTunnelConfig,
    loader: &ReverseTunnelLoaderSnapshot,
    runtime: Runtime,
) -> Result<PreparedReverseTunnel, AnyError> {
    let mut keys = HashSet::new();
    let mut initiators = Vec::with_capacity(config.initiator.len());
    for config in config.initiator {
        let builder = ReverseTunnelInitiatorBuilder::new(config, runtime.clone())?;
        if !keys.insert(builder.key.clone()) {
            return Err(BuildError::DuplicateKey(builder.key).into());
        }
        initiators.push(builder);
    }
    let mut responder_keys = HashSet::new();
    let mut tcp_responders = Vec::new();
    let mut rtp_responders = Vec::new();
    for config in config.responder {
        let listen_addr = config.listen_addr.0;
        let transport = responder_transport(&listen_addr)?;
        let key: Arc<str> = Arc::from(listen_addr.to_string());
        if !responder_keys.insert(key.clone()) {
            return Err(BuildError::DuplicateKey(key).into());
        }
        match transport {
            ReverseTunnelTransport::Tcp => tcp_responders.push(TcpReverseTunnelResponderBuilder {
                key,
                listen_addr,
                header_key: config.header_key,
                runtime: runtime.clone(),
            }),
            ReverseTunnelTransport::Rtp => rtp_responders.push(RtpReverseTunnelResponderBuilder {
                key,
                listen_addr,
                header_key: config.header_key,
                runtime: runtime.clone(),
            }),
        }
    }
    Ok(PreparedReverseTunnel {
        initiator: loader.initiator.prepare(initiators).await?,
        tcp_responder: loader.tcp_responder.prepare(tcp_responders).await?,
        rtp_responder: loader.rtp_responder.prepare(rtp_responders).await?,
    })
}
