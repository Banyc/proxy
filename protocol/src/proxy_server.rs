use std::convert::Infallible;

use crate::{
    stream::streams::{
        kcp::KcpProxyServerConfig, mptcp::MptcpProxyServerConfig, rtp::RtpProxyServerConfig,
        rtp_mux::RtpMuxProxyServerConfig, tcp::proxy_server::TcpProxyServerConfig,
        tcp_mux::TcpMuxProxyServerConfig,
    },
    udp::proxy_server::{UdpProxyServerBuilder, UdpProxyServerConfig},
};
use common::{
    config::Merge,
    error::{AnyError, AnyResult},
    loading,
    proto::{
        conn_handler::{stream::StreamProxyConnHandler, udp::UdpProxyConnHandler},
        context::Runtime,
    },
};
use serde::Deserialize;

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct ProxyServerConfig {
    #[serde(default)]
    pub tcp_server: Vec<TcpProxyServerConfig>,
    #[serde(default)]
    pub tcp_mux_server: Vec<TcpMuxProxyServerConfig>,
    #[serde(default)]
    pub udp_server: Vec<UdpProxyServerConfig>,
    #[serde(default)]
    pub kcp_server: Vec<KcpProxyServerConfig>,
    #[serde(default)]
    pub mptcp_server: Vec<MptcpProxyServerConfig>,
    #[serde(default)]
    pub rtp_server: Vec<RtpProxyServerConfig>,
    #[serde(default)]
    pub rtp_mux_server: Vec<RtpMuxProxyServerConfig>,
}
impl ProxyServerConfig {
    pub fn new() -> Self {
        Self {
            tcp_server: Default::default(),
            tcp_mux_server: Default::default(),
            udp_server: Default::default(),
            kcp_server: Default::default(),
            mptcp_server: Default::default(),
            rtp_server: Default::default(),
            rtp_mux_server: Default::default(),
        }
    }
}
impl Merge for ProxyServerConfig {
    type Error = Infallible;

    fn merge(mut self, other: Self) -> Result<Self, Self::Error>
    where
        Self: Sized,
    {
        self.tcp_server.extend(other.tcp_server);
        self.tcp_mux_server.extend(other.tcp_mux_server);
        self.udp_server.extend(other.udp_server);
        self.kcp_server.extend(other.kcp_server);
        self.mptcp_server.extend(other.mptcp_server);
        self.rtp_server.extend(other.rtp_server);
        self.rtp_mux_server.extend(other.rtp_mux_server);
        Ok(Self {
            tcp_server: self.tcp_server,
            tcp_mux_server: self.tcp_mux_server,
            udp_server: self.udp_server,
            kcp_server: self.kcp_server,
            mptcp_server: self.mptcp_server,
            rtp_server: self.rtp_server,
            rtp_mux_server: self.rtp_mux_server,
        })
    }
}

#[derive(Debug)]
pub struct ProxyServerLoader {
    pub tcp_server: loading::Loader<StreamProxyConnHandler>,
    pub tcp_mux_server: loading::Loader<StreamProxyConnHandler>,
    pub udp_server: loading::Loader<UdpProxyConnHandler>,
    pub kcp_server: loading::Loader<StreamProxyConnHandler>,
    pub mptcp_server: loading::Loader<StreamProxyConnHandler>,
    pub rtp_server: loading::Loader<StreamProxyConnHandler>,
    pub rtp_mux_server: loading::Loader<StreamProxyConnHandler>,
}
impl ProxyServerLoader {
    pub fn new() -> Self {
        Self {
            tcp_server: loading::Loader::new(),
            tcp_mux_server: loading::Loader::new(),
            udp_server: loading::Loader::new(),
            kcp_server: loading::Loader::new(),
            mptcp_server: loading::Loader::new(),
            rtp_server: loading::Loader::new(),
            rtp_mux_server: loading::Loader::new(),
        }
    }

    /// Commit a previously-prepared proxy-server reload: hot-swap handlers
    /// on existing listeners, spawn new listener tasks, and drop handles for
    /// removed listeners. Returns an error if a listener died between
    /// prepare and commit (a handler update would be silently lost).
    pub fn commit(
        &mut self,
        join_set: &mut tokio::task::JoinSet<AnyResult>,
        prepared: PreparedProxyServer,
    ) -> AnyResult {
        self.tcp_server.commit(join_set, prepared.tcp_server)?;
        self.tcp_mux_server
            .commit(join_set, prepared.tcp_mux_server)?;
        self.udp_server.commit(join_set, prepared.udp_server)?;
        self.kcp_server.commit(join_set, prepared.kcp_server)?;
        self.mptcp_server.commit(join_set, prepared.mptcp_server)?;
        self.rtp_server.commit(join_set, prepared.rtp_server)?;
        self.rtp_mux_server
            .commit(join_set, prepared.rtp_mux_server)?;
        Ok(())
    }
}
impl Default for ProxyServerLoader {
    fn default() -> Self {
        Self::new()
    }
}

/// A fully-prepared proxy-server reload: bound listener sockets and built
/// handlers for every proxy-server kind, ready to commit. Dropping it without
/// [`ProxyServerLoader::commit`] drops the bound sockets — live state is
/// untouched.
pub struct PreparedProxyServer {
    tcp_server: loading::PreparedOps<StreamProxyConnHandler>,
    tcp_mux_server: loading::PreparedOps<StreamProxyConnHandler>,
    udp_server: loading::PreparedOps<UdpProxyConnHandler>,
    kcp_server: loading::PreparedOps<StreamProxyConnHandler>,
    mptcp_server: loading::PreparedOps<StreamProxyConnHandler>,
    rtp_server: loading::PreparedOps<StreamProxyConnHandler>,
    rtp_mux_server: loading::PreparedOps<StreamProxyConnHandler>,
}

/// Prepare a proxy-server reload: build every listener (binding sockets) and
/// handler without touching live state. On any failure the returned `Err`
/// drops everything already prepared, leaving the live configuration
/// untouched.
pub async fn prepare(
    config: ProxyServerConfig,
    loader: &ProxyServerLoader,
    context: Runtime,
) -> Result<PreparedProxyServer, AnyError> {
    let tcp_server = tcp_prepare(config.tcp_server, &loader.tcp_server, &context).await?;
    let tcp_mux_server =
        tcp_mux_prepare(config.tcp_mux_server, &loader.tcp_mux_server, &context).await?;
    let udp_server = udp_prepare(config.udp_server, &loader.udp_server, &context).await?;
    let kcp_server = kcp_prepare(config.kcp_server, &loader.kcp_server, &context).await?;
    let mptcp_server = mptcp_prepare(config.mptcp_server, &loader.mptcp_server, &context).await?;
    let rtp_server = rtp_prepare(config.rtp_server, &loader.rtp_server, &context).await?;
    let rtp_mux_server =
        rtp_mux_prepare(config.rtp_mux_server, &loader.rtp_mux_server, &context).await?;
    Ok(PreparedProxyServer {
        tcp_server,
        tcp_mux_server,
        udp_server,
        kcp_server,
        mptcp_server,
        rtp_server,
        rtp_mux_server,
    })
}
async fn tcp_prepare(
    config: Vec<TcpProxyServerConfig>,
    loader: &loading::Loader<StreamProxyConnHandler>,
    context: &Runtime,
) -> Result<loading::PreparedOps<StreamProxyConnHandler>, AnyError> {
    loader
        .prepare(
            config
                .into_iter()
                .map(|s| s.into_builder(context.stream.clone()))
                .collect(),
        )
        .await
}
async fn tcp_mux_prepare(
    config: Vec<TcpMuxProxyServerConfig>,
    loader: &loading::Loader<StreamProxyConnHandler>,
    context: &Runtime,
) -> Result<loading::PreparedOps<StreamProxyConnHandler>, AnyError> {
    loader
        .prepare(
            config
                .into_iter()
                .map(|s| s.into_builder(context.stream.clone()))
                .collect(),
        )
        .await
}
async fn udp_prepare(
    config: Vec<UdpProxyServerConfig>,
    loader: &loading::Loader<UdpProxyConnHandler>,
    context: &Runtime,
) -> Result<loading::PreparedOps<UdpProxyConnHandler>, AnyError> {
    loader
        .prepare(
            config
                .into_iter()
                .map(|config| UdpProxyServerBuilder {
                    config,
                    udp_context: context.udp.clone(),
                })
                .collect(),
        )
        .await
}
async fn kcp_prepare(
    config: Vec<KcpProxyServerConfig>,
    loader: &loading::Loader<StreamProxyConnHandler>,
    context: &Runtime,
) -> Result<loading::PreparedOps<StreamProxyConnHandler>, AnyError> {
    loader
        .prepare(
            config
                .into_iter()
                .map(|s| s.into_builder(context.stream.clone()))
                .collect(),
        )
        .await
}
async fn mptcp_prepare(
    config: Vec<MptcpProxyServerConfig>,
    loader: &loading::Loader<StreamProxyConnHandler>,
    context: &Runtime,
) -> Result<loading::PreparedOps<StreamProxyConnHandler>, AnyError> {
    loader
        .prepare(
            config
                .into_iter()
                .map(|s| s.into_builder(context.stream.clone()))
                .collect(),
        )
        .await
}
async fn rtp_prepare(
    config: Vec<RtpProxyServerConfig>,
    loader: &loading::Loader<StreamProxyConnHandler>,
    context: &Runtime,
) -> Result<loading::PreparedOps<StreamProxyConnHandler>, AnyError> {
    loader
        .prepare(
            config
                .into_iter()
                .map(|s| s.into_builder(context.stream.clone()))
                .collect(),
        )
        .await
}
async fn rtp_mux_prepare(
    config: Vec<RtpMuxProxyServerConfig>,
    loader: &loading::Loader<StreamProxyConnHandler>,
    context: &Runtime,
) -> Result<loading::PreparedOps<StreamProxyConnHandler>, AnyError> {
    loader
        .prepare(
            config
                .into_iter()
                .map(|s| s.into_builder(context.stream.clone()))
                .collect(),
        )
        .await
}
