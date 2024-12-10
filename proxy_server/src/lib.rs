use std::{convert::Infallible, io};

use common::{config::Merge, error::AnyResult, loading};
use protocol::context::ConcreteContext;
use serde::Deserialize;
use stream::{
    kcp::KcpProxyServerConfig, mptcp::MptcpProxyServerConfig, rtp::RtpProxyServerConfig,
    tcp::TcpProxyServerConfig, StreamProxyServer,
};
use thiserror::Error;
use udp::{UdpProxy, UdpProxyServerBuilder, UdpProxyServerConfig};

pub mod stream;
pub mod udp;

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct ProxyServerConfig {
    #[serde(default)]
    pub tcp_server: Vec<TcpProxyServerConfig>,
    #[serde(default)]
    pub udp_server: Vec<UdpProxyServerConfig>,
    #[serde(default)]
    pub kcp_server: Vec<KcpProxyServerConfig>,
    #[serde(default)]
    pub mptcp_server: Vec<MptcpProxyServerConfig>,
    #[serde(default)]
    pub rtp_server: Vec<RtpProxyServerConfig>,
}
impl ProxyServerConfig {
    pub fn new() -> Self {
        Self {
            tcp_server: Default::default(),
            udp_server: Default::default(),
            kcp_server: Default::default(),
            mptcp_server: Default::default(),
            rtp_server: Default::default(),
        }
    }

    pub async fn spawn_and_clean(
        self,
        join_set: &mut tokio::task::JoinSet<AnyResult>,
        loader: &mut ProxyServerLoader,
        context: ConcreteContext,
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
        self.udp_server.extend(other.udp_server);
        self.kcp_server.extend(other.kcp_server);
        self.mptcp_server.extend(other.mptcp_server);
        self.rtp_server.extend(other.rtp_server);
        Ok(Self {
            tcp_server: self.tcp_server,
            udp_server: self.udp_server,
            kcp_server: self.kcp_server,
            mptcp_server: self.mptcp_server,
            rtp_server: self.rtp_server,
        })
    }
}

pub struct ProxyServerLoader {
    tcp_server: loading::Loader<StreamProxyServer>,
    udp_server: loading::Loader<UdpProxy>,
    kcp_server: loading::Loader<StreamProxyServer>,
    mptcp_server: loading::Loader<StreamProxyServer>,
    rtp_server: loading::Loader<StreamProxyServer>,
}
impl ProxyServerLoader {
    pub fn new() -> Self {
        Self {
            tcp_server: loading::Loader::new(),
            udp_server: loading::Loader::new(),
            kcp_server: loading::Loader::new(),
            mptcp_server: loading::Loader::new(),
            rtp_server: loading::Loader::new(),
        }
    }
}
impl Default for ProxyServerLoader {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug, Error)]
#[error("Failed to bind to listen address: {0}")]
pub struct ListenerBindError(#[source] io::Error);
