use std::{collections::HashMap, io, sync::Arc};

use async_speed_limit::Limiter;
use common::{
    addr::{InternetAddr, InternetAddrStr},
    config::SharableConfig,
    loading,
    proxy_table::ProxyGroupBuildError,
    udp::{
        context::UdpContext,
        io_copy::{CopyBiError, CopyBidirectional, DownstreamParts, UpstreamParts},
        proxy_table::{UdpProxyGroup, UdpProxyGroupBuilder},
        Flow, Packet, UdpServer, UdpServerHook, UpstreamAddr,
    },
};
use proxy_client::udp::{EstablishError, UdpProxyClient};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::net::ToSocketAddrs;
use tracing::{error, warn};
use udp_listener::AcceptedUdp;

use self::proxy_table::UdpProxyGroupBuildContext;

pub mod proxy_table;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UdpAccessServerConfig {
    pub listen_addr: Arc<str>,
    pub destination: InternetAddrStr,
    pub proxy_group: SharableConfig<UdpProxyGroupBuilder>,
    pub speed_limit: Option<f64>,
}

impl UdpAccessServerConfig {
    pub fn into_builder(
        self,
        proxy_group: &HashMap<Arc<str>, UdpProxyGroup>,
        cx: UdpProxyGroupBuildContext<'_>,
        udp_context: UdpContext,
    ) -> Result<UdpAccessServerBuilder, BuildError> {
        let proxy_group = match self.proxy_group {
            SharableConfig::SharingKey(key) => proxy_group
                .get(&key)
                .ok_or_else(|| BuildError::ProxyGroupKeyNotFound(key.clone()))?
                .clone(),
            SharableConfig::Private(x) => x.build(cx)?,
        };

        Ok(UdpAccessServerBuilder {
            listen_addr: self.listen_addr,
            destination: self.destination,
            proxy_group,
            speed_limit: self.speed_limit.unwrap_or(f64::INFINITY),
            udp_context,
        })
    }
}

#[derive(Debug, Error)]
pub enum BuildError {
    #[error("Proxy group key not found: {0}")]
    ProxyGroupKeyNotFound(Arc<str>),
    #[error("{0}")]
    ProxyGroup(#[from] ProxyGroupBuildError),
}

#[derive(Debug, Clone)]
pub struct UdpAccessServerBuilder {
    listen_addr: Arc<str>,
    destination: InternetAddrStr,
    proxy_group: UdpProxyGroup,
    speed_limit: f64,
    udp_context: UdpContext,
}

impl loading::Builder for UdpAccessServerBuilder {
    type Hook = UdpAccess;
    type Server = UdpServer<Self::Hook>;
    type Err = io::Error;

    async fn build_server(self) -> Result<Self::Server, Self::Err> {
        let listen_addr = self.listen_addr.clone();
        let access = self.build_hook()?;
        let server = access.build(listen_addr.as_ref()).await?;
        Ok(server)
    }

    fn key(&self) -> &Arc<str> {
        &self.listen_addr
    }

    fn build_hook(self) -> Result<Self::Hook, Self::Err> {
        Ok(UdpAccess::new(
            self.proxy_group,
            self.destination.0,
            self.speed_limit,
            self.udp_context,
        ))
    }
}

#[derive(Debug)]
pub struct UdpAccess {
    proxy_group: UdpProxyGroup,
    destination: InternetAddr,
    speed_limiter: Limiter,
    udp_context: UdpContext,
}

impl loading::Hook for UdpAccess {}

impl UdpAccess {
    pub fn new(
        proxy_table: UdpProxyGroup,
        destination: InternetAddr,
        speed_limit: f64,
        udp_context: UdpContext,
    ) -> Self {
        Self {
            proxy_group: proxy_table,
            destination,
            speed_limiter: Limiter::new(speed_limit),
            udp_context,
        }
    }

    pub async fn build(self, listen_addr: impl ToSocketAddrs) -> io::Result<UdpServer<Self>> {
        let listener = tokio::net::UdpSocket::bind(listen_addr).await?;
        Ok(UdpServer::new(listener, self))
    }

    async fn proxy(&self, accepted_udp: AcceptedUdp<Flow, Packet>) -> Result<(), AccessProxyError> {
        // Connect to upstream
        let proxy_chain = self.proxy_group.choose_chain();
        let upstream =
            UdpProxyClient::establish(proxy_chain.chain.clone(), self.destination.clone()).await?;
        let upstream_remote = upstream.remote_addr().clone();

        let (upstream_read, upstream_write) = upstream.into_split();

        let speed_limiter = self.speed_limiter.clone();
        let payload_crypto = proxy_chain.payload_crypto.clone();
        let session_table = self.udp_context.session_table.clone();
        let upstream_local = upstream_read.inner().local_addr().ok();
        let flow = accepted_udp.dispatch_key().clone();
        let (dn_read, dn_write) = accepted_udp.split();
        tokio::spawn(async move {
            let io_copy = CopyBidirectional {
                flow,
                upstream: UpstreamParts {
                    read: upstream_read,
                    write: upstream_write,
                },
                downstream: DownstreamParts {
                    read: dn_read,
                    write: dn_write,
                },
                speed_limiter,
                payload_crypto,
                response_header: None,
            };
            let _ = io_copy
                .serve_as_access_server(session_table, upstream_local, upstream_remote, "UDP")
                .await;
        });
        Ok(())
    }
}

#[derive(Debug, Error)]
pub enum AccessProxyError {
    #[error("Failed to establish proxy chain: {0}")]
    Establish(#[from] EstablishError),
    #[error("Failed to copy: {0}")]
    Copy(#[from] CopyBiError),
}

impl UdpServerHook for UdpAccess {
    fn parse_upstream_addr(&self, _buf: &mut io::Cursor<&[u8]>) -> Option<Option<UpstreamAddr>> {
        Some(Some(UpstreamAddr(self.destination.clone())))
    }

    async fn handle_flow(&self, accepted: AcceptedUdp<common::udp::Flow, Packet>) {
        let res = self.proxy(accepted).await;
        match res {
            Ok(()) => (),
            Err(e) => {
                warn!(?e, "Failed to proxy");
            }
        }
    }
}
