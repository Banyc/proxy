use std::{collections::HashMap, io, net::Ipv4Addr, sync::Arc};

use async_speed_limit::Limiter;
use async_trait::async_trait;
use common::{
    addr::any_addr,
    addr::InternetAddr,
    config::SharableConfig,
    loading,
    udp::{
        io_copy::{copy_bidirectional, CopyBiError, DownstreamParts, UpstreamParts},
        proxy_table::UdpProxyTable,
        Flow, FlowMetrics, Packet, UdpDownstreamWriter, UdpServer, UdpServerHook, UpstreamAddr,
    },
};
use proxy_client::udp::{EstablishError, UdpProxyClient};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::{net::ToSocketAddrs, sync::mpsc};
use tracing::{error, info, warn};

use self::proxy_table::UdpProxyTableBuilder;

pub mod proxy_table;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UdpAccessServerConfig {
    pub listen_addr: Arc<str>,
    pub destination: Arc<str>,
    pub proxy_table: SharableConfig<UdpProxyTableBuilder>,
    pub speed_limit: Option<f64>,
}

impl UdpAccessServerConfig {
    pub fn into_builder(
        self,
        proxy_tables: &HashMap<Arc<str>, UdpProxyTable>,
    ) -> Result<UdpAccessServerBuilder, BuildError> {
        let proxy_table = match self.proxy_table {
            SharableConfig::SharingKey(key) => proxy_tables
                .get(&key)
                .ok_or_else(|| BuildError::ProxyTableKeyNotFound(key.clone()))?
                .clone(),
            SharableConfig::Private(x) => x.build(),
        };

        Ok(UdpAccessServerBuilder {
            listen_addr: self.listen_addr,
            destination: self.destination,
            proxy_table,
            speed_limit: self.speed_limit.unwrap_or(f64::INFINITY),
        })
    }
}

#[derive(Debug, Error)]
pub enum BuildError {
    #[error("Proxy table key not found: {0}")]
    ProxyTableKeyNotFound(Arc<str>),
}

#[derive(Debug, Clone)]
pub struct UdpAccessServerBuilder {
    listen_addr: Arc<str>,
    destination: Arc<str>,
    proxy_table: UdpProxyTable,
    speed_limit: f64,
}

#[async_trait]
impl loading::Builder for UdpAccessServerBuilder {
    type Hook = UdpAccess;
    type Server = UdpServer<Self::Hook>;

    async fn build_server(self) -> io::Result<UdpServer<UdpAccess>> {
        let listen_addr = self.listen_addr.clone();
        let access = self.build_hook()?;
        let server = access.build(listen_addr.as_ref()).await?;
        Ok(server)
    }

    fn key(&self) -> &Arc<str> {
        &self.listen_addr
    }

    fn build_hook(self) -> io::Result<UdpAccess> {
        Ok(UdpAccess::new(
            self.proxy_table,
            self.destination.into(),
            self.speed_limit,
        ))
    }
}

#[derive(Debug)]
pub struct UdpAccess {
    proxy_table: UdpProxyTable,
    destination: InternetAddr,
    speed_limiter: Limiter,
}

impl loading::Hook for UdpAccess {}

impl UdpAccess {
    pub fn new(proxy_table: UdpProxyTable, destination: InternetAddr, speed_limit: f64) -> Self {
        Self {
            proxy_table,
            destination,
            speed_limiter: Limiter::new(speed_limit),
        }
    }

    pub async fn build(self, listen_addr: impl ToSocketAddrs) -> io::Result<UdpServer<Self>> {
        let listener = tokio::net::UdpSocket::bind(listen_addr)
            .await
            .map_err(|e| {
                error!(?e, "Failed to bind to listen address");
                e
            })?;
        Ok(UdpServer::new(listener, self))
    }

    async fn proxy(
        &self,
        rx: mpsc::Receiver<Packet>,
        flow: Flow,
        downstream_writer: UdpDownstreamWriter,
    ) -> Result<FlowMetrics, AccessProxyError> {
        // Connect to upstream
        let proxy_chain = self.proxy_table.choose_chain();
        let upstream =
            UdpProxyClient::establish(proxy_chain.chain.clone(), self.destination.clone()).await?;

        let (upstream_read, upstream_write) = upstream.into_split();

        let metrics = copy_bidirectional(
            flow,
            UpstreamParts {
                read: upstream_read,
                write: upstream_write,
            },
            DownstreamParts {
                rx,
                write: downstream_writer,
            },
            self.speed_limiter.clone(),
            proxy_chain.payload_crypto.clone(),
            None,
        )
        .await?;
        Ok(metrics)
    }
}

#[derive(Debug, Error)]
pub enum AccessProxyError {
    #[error("Failed to establish proxy chain")]
    Establish(#[from] EstablishError),
    #[error("Failed to copy")]
    Copy(#[from] CopyBiError),
}

#[async_trait]
impl UdpServerHook for UdpAccess {
    async fn parse_upstream_addr<'buf>(
        &self,
        buf: &'buf [u8],
        _downstream_writer: &UdpDownstreamWriter,
    ) -> Option<(UpstreamAddr, &'buf [u8])> {
        Some((
            UpstreamAddr(any_addr(&Ipv4Addr::UNSPECIFIED.into()).into()),
            buf,
        ))
    }

    async fn handle_flow(
        &self,
        rx: mpsc::Receiver<Packet>,
        flow: Flow,
        downstream_writer: UdpDownstreamWriter,
    ) {
        let res = self.proxy(rx, flow, downstream_writer).await;
        match res {
            Ok(metrics) => {
                info!(%metrics, "Proxy finished");
            }
            Err(e) => {
                warn!(?e, "Failed to proxy");
            }
        }
    }
}
