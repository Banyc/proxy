use std::{collections::HashMap, io, sync::Arc};

use async_speed_limit::Limiter;
use common::{
    addr::{InternetAddr, InternetAddrStr},
    config::SharableConfig,
    loading,
    udp::{
        context::UdpContext,
        io_copy::{CopyBiError, CopyBidirectional, DownstreamParts, UpstreamParts},
        proxy_table::{UdpProxyConfig, UdpProxyTable},
        FlowOwnedGuard, Packet, UdpDownstreamWriter, UdpServer, UdpServerHook, UpstreamAddr,
    },
};
use proxy_client::udp::{EstablishError, UdpProxyClient};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::{net::ToSocketAddrs, sync::mpsc};
use tokio_util::sync::CancellationToken;
use tracing::{error, warn};

use self::proxy_table::{UdpProxyTableBuildError, UdpProxyTableBuilder};

pub mod proxy_table;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UdpAccessServerConfig {
    pub listen_addr: Arc<str>,
    pub destination: InternetAddrStr,
    pub proxy_table: SharableConfig<UdpProxyTableBuilder>,
    pub speed_limit: Option<f64>,
}

impl UdpAccessServerConfig {
    pub fn into_builder(
        self,
        udp_proxy: &HashMap<Arc<str>, UdpProxyConfig>,
        proxy_tables: &HashMap<Arc<str>, UdpProxyTable>,
        cancellation: CancellationToken,
        udp_context: UdpContext,
    ) -> Result<UdpAccessServerBuilder, BuildError> {
        let proxy_table = match self.proxy_table {
            SharableConfig::SharingKey(key) => proxy_tables
                .get(&key)
                .ok_or_else(|| BuildError::ProxyTableKeyNotFound(key.clone()))?
                .clone(),
            SharableConfig::Private(x) => x.build(udp_proxy, cancellation.clone())?,
        };

        Ok(UdpAccessServerBuilder {
            listen_addr: self.listen_addr,
            destination: self.destination,
            proxy_table,
            speed_limit: self.speed_limit.unwrap_or(f64::INFINITY),
            udp_context,
        })
    }
}

#[derive(Debug, Error)]
pub enum BuildError {
    #[error("Proxy table key not found: {0}")]
    ProxyTableKeyNotFound(Arc<str>),
    #[error("{0}")]
    ProxyTable(#[from] UdpProxyTableBuildError),
}

#[derive(Debug, Clone)]
pub struct UdpAccessServerBuilder {
    listen_addr: Arc<str>,
    destination: InternetAddrStr,
    proxy_table: UdpProxyTable,
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
            self.proxy_table,
            self.destination.0,
            self.speed_limit,
            self.udp_context,
        ))
    }
}

#[derive(Debug)]
pub struct UdpAccess {
    proxy_table: UdpProxyTable,
    destination: InternetAddr,
    speed_limiter: Limiter,
    udp_context: UdpContext,
}

impl loading::Hook for UdpAccess {}

impl UdpAccess {
    pub fn new(
        proxy_table: UdpProxyTable,
        destination: InternetAddr,
        speed_limit: f64,
        udp_context: UdpContext,
    ) -> Self {
        Self {
            proxy_table,
            destination,
            speed_limiter: Limiter::new(speed_limit),
            udp_context,
        }
    }

    pub async fn build(self, listen_addr: impl ToSocketAddrs) -> io::Result<UdpServer<Self>> {
        let listener = tokio::net::UdpSocket::bind(listen_addr).await?;
        Ok(UdpServer::new(listener, self))
    }

    async fn proxy(
        &self,
        rx: mpsc::Receiver<Packet>,
        flow: FlowOwnedGuard,
        downstream_writer: UdpDownstreamWriter,
    ) -> Result<(), AccessProxyError> {
        // Connect to upstream
        let proxy_chain = self.proxy_table.choose_chain();
        let upstream =
            UdpProxyClient::establish(proxy_chain.chain.clone(), self.destination.clone()).await?;
        let upstream_remote = upstream.remote_addr().clone();

        let (upstream_read, upstream_write) = upstream.into_split();

        let speed_limiter = self.speed_limiter.clone();
        let payload_crypto = proxy_chain.payload_crypto.clone();
        let session_table = self.udp_context.session_table.clone();
        let upstream_local = upstream_read.inner().local_addr().ok();
        tokio::spawn(async move {
            let io_copy = CopyBidirectional {
                flow,
                upstream: UpstreamParts {
                    read: upstream_read,
                    write: upstream_write,
                },
                downstream: DownstreamParts {
                    rx,
                    write: downstream_writer,
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
    async fn parse_upstream_addr(
        &self,
        _buf: &mut io::Cursor<&[u8]>,
        _downstream_writer: &UdpDownstreamWriter,
    ) -> Option<UpstreamAddr> {
        Some(UpstreamAddr(self.destination.clone()))
    }

    async fn handle_flow(
        &self,
        rx: mpsc::Receiver<Packet>,
        flow: FlowOwnedGuard,
        downstream_writer: UdpDownstreamWriter,
    ) {
        let res = self.proxy(rx, flow, downstream_writer).await;
        match res {
            Ok(()) => (),
            Err(e) => {
                warn!(?e, "Failed to proxy");
            }
        }
    }
}
