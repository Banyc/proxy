use std::convert::Infallible;

use crate::stream::streams::{
    kcp::KcpProxyServerConfig, mptcp::MptcpProxyServerConfig, rtp::RtpProxyServerConfig,
    rtp_mux::RtpMuxProxyServerConfig, tcp::TcpProxyServerConfig, tcp_mux::TcpMuxProxyServerConfig,
};
use common::{
    config::Merge,
    error::AnyResult,
    loading,
    proto::{
        conn_handler::{
            stream::StreamProxyConnHandler,
            udp::{UdpProxyConnHandler, UdpProxyServerBuilder, UdpProxyServerConfig},
        },
        context::Context,
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

    pub async fn spawn_and_clean(
        self,
        join_set: &mut tokio::task::JoinSet<AnyResult>,
        loader: &mut ProxyServerLoader,
        context: Context,
    ) -> AnyResult {
        loader
            .tcp_server
            .spawn_and_clean(
                join_set,
                self.tcp_server
                    .into_iter()
                    .map(|s| s.into_builder(context.stream.clone()))
                    .collect(),
            )
            .await?;

        loader
            .tcp_mux_server
            .spawn_and_clean(
                join_set,
                self.tcp_mux_server
                    .into_iter()
                    .map(|s| s.into_builder(context.stream.clone()))
                    .collect(),
            )
            .await?;

        loader
            .udp_server
            .spawn_and_clean(
                join_set,
                self.udp_server
                    .into_iter()
                    .map(|config| UdpProxyServerBuilder {
                        config,
                        udp_context: context.udp.clone(),
                    })
                    .collect(),
            )
            .await?;

        loader
            .kcp_server
            .spawn_and_clean(
                join_set,
                self.kcp_server
                    .into_iter()
                    .map(|s| s.into_builder(context.stream.clone()))
                    .collect(),
            )
            .await?;

        loader
            .mptcp_server
            .spawn_and_clean(
                join_set,
                self.mptcp_server
                    .into_iter()
                    .map(|s| s.into_builder(context.stream.clone()))
                    .collect(),
            )
            .await?;

        loader
            .rtp_server
            .spawn_and_clean(
                join_set,
                self.rtp_server
                    .into_iter()
                    .map(|s| s.into_builder(context.stream.clone()))
                    .collect(),
            )
            .await?;

        loader
            .rtp_mux_server
            .spawn_and_clean(
                join_set,
                self.rtp_mux_server
                    .into_iter()
                    .map(|s| s.into_builder(context.stream.clone()))
                    .collect(),
            )
            .await?;

        Ok(())
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

pub struct ProxyServerLoader {
    tcp_server: loading::Loader<StreamProxyConnHandler>,
    tcp_mux_server: loading::Loader<StreamProxyConnHandler>,
    udp_server: loading::Loader<UdpProxyConnHandler>,
    kcp_server: loading::Loader<StreamProxyConnHandler>,
    mptcp_server: loading::Loader<StreamProxyConnHandler>,
    rtp_server: loading::Loader<StreamProxyConnHandler>,
    rtp_mux_server: loading::Loader<StreamProxyConnHandler>,
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
}
impl Default for ProxyServerLoader {
    fn default() -> Self {
        Self::new()
    }
}
