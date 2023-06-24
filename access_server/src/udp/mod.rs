use std::{collections::HashMap, io, net::Ipv4Addr, sync::Arc};

use async_trait::async_trait;
use common::{
    addr::any_addr,
    addr::InternetAddr,
    config::SharableConfig,
    loading,
    udp::{
        proxy_table::UdpProxyTable, Flow, FlowMetrics, Packet, UdpDownstreamWriter, UdpServer,
        UdpServerHook, UpstreamAddr, BUFFER_LENGTH, LIVE_CHECK_INTERVAL, TIMEOUT,
    },
};
use proxy_client::udp::{EstablishError, RecvError, SendError, UdpProxyClient};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::{net::ToSocketAddrs, sync::mpsc};
use tracing::{error, info, trace, warn};

use self::proxy_table::UdpProxyTableBuilder;

pub mod proxy_table;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UdpAccessServerConfig {
    listen_addr: Arc<str>,
    destination: Arc<str>,
    proxy_table: SharableConfig<UdpProxyTableBuilder>,
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
        Ok(UdpAccess::new(self.proxy_table, self.destination.into()))
    }
}

#[derive(Debug)]
pub struct UdpAccess {
    proxy_table: UdpProxyTable,
    destination: InternetAddr,
}

impl loading::Hook for UdpAccess {}

impl UdpAccess {
    pub fn new(proxy_table: UdpProxyTable, destination: InternetAddr) -> Self {
        Self {
            proxy_table,
            destination,
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
        mut rx: mpsc::Receiver<Packet>,
        flow: Flow,
        downstream_writer: UdpDownstreamWriter,
    ) -> Result<FlowMetrics, ProxyError> {
        let start = std::time::Instant::now();

        // Connect to upstream
        let proxy_chain = self.proxy_table.choose_chain();
        let mut upstream =
            UdpProxyClient::establish(proxy_chain.chain.clone(), self.destination.clone()).await?;

        // Periodic check if the flow is still alive
        let mut tick = tokio::time::interval(LIVE_CHECK_INTERVAL);
        let mut last_packet = std::time::Instant::now();

        let mut bytes_uplink = 0;
        let mut bytes_downlink = 0;
        let mut packets_uplink = 0;
        let mut packets_downlink = 0;

        // Forward packets
        let mut downlink_buf = [0; BUFFER_LENGTH];
        loop {
            trace!("Waiting for packet");
            tokio::select! {
                res = rx.recv() => {
                    trace!("Received packet from downstream");
                    let packet = match res {
                        Some(packet) => packet,
                        None => {
                            // Channel closed
                            break;
                        }
                    };

                    // Send packet to upstream
                    upstream.send(&packet.0).await?;
                    bytes_uplink += packet.0.len() as u64;
                    packets_uplink += 1;

                    last_packet = std::time::Instant::now();
                }
                res = upstream.recv(&mut downlink_buf) => {
                    trace!("Received packet from upstream");
                    let n = res?;
                    let pkt = &downlink_buf[..n];

                    // Send packet to downstream
                    downstream_writer.send(pkt).await.map_err(|e| ProxyError::SendDownstream { source: e, downstream: downstream_writer.clone() })?;
                    bytes_downlink += pkt.len() as u64;
                    packets_downlink += 1;

                    last_packet = std::time::Instant::now();
                }
                _ = tick.tick() => {
                    trace!("Checking if flow is still alive");
                    if last_packet.elapsed() > TIMEOUT {
                        trace!(?flow, "Flow timed out");
                        break;
                    }
                }
            }
        }

        Ok(FlowMetrics {
            flow,
            start,
            end: last_packet,
            bytes_uplink,
            bytes_downlink,
            packets_uplink,
            packets_downlink,
        })
    }
}

#[derive(Debug, Error)]
pub enum ProxyError {
    #[error("Failed to establish proxy chain")]
    EstablishError(#[from] EstablishError),
    #[error("Failed to send to upstream")]
    SendUpstream(#[from] SendError),
    #[error("Failed to recv from upstream")]
    RecvUpstream(#[from] RecvError),
    #[error("Failed to send to downstream")]
    SendDownstream {
        #[source]
        source: io::Error,
        downstream: UdpDownstreamWriter,
    },
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
