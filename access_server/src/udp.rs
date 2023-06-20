use std::{io, net::Ipv4Addr, sync::Arc};

use async_trait::async_trait;
use common::{
    addr::any_addr,
    addr::InternetAddr,
    loading,
    udp::{
        proxy_table::{UdpProxyTable, UdpProxyTableBuilder},
        Flow, FlowMetrics, Packet, UdpDownstreamWriter, UdpServer, UdpServerHook, UpstreamAddr,
        BUFFER_LENGTH, LIVE_CHECK_INTERVAL, TIMEOUT,
    },
};
use proxy_client::udp::{EstablishError, RecvError, SendError, UdpProxySocket, UdpTracer};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::{net::ToSocketAddrs, sync::mpsc};
use tracing::{error, info, trace, warn};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UdpProxyAccessBuilder {
    listen_addr: Arc<str>,
    proxy_table: UdpProxyTableBuilder,
    destination: Arc<str>,
}

#[async_trait]
impl loading::Builder for UdpProxyAccessBuilder {
    type Hook = UdpProxyAccess;
    type Server = UdpServer<Self::Hook>;

    async fn build_server(self) -> io::Result<UdpServer<UdpProxyAccess>> {
        let access = UdpProxyAccess::new(
            self.proxy_table.build::<UdpTracer>(None),
            self.destination.into(),
        );
        let server = access.build(self.listen_addr.as_ref()).await?;
        Ok(server)
    }

    fn key(&self) -> &Arc<str> {
        &self.listen_addr
    }

    fn build_hook(self) -> io::Result<UdpProxyAccess> {
        Ok(UdpProxyAccess::new(
            self.proxy_table.build::<UdpTracer>(None),
            self.destination.into(),
        ))
    }
}

#[derive(Debug)]
pub struct UdpProxyAccess {
    proxy_table: UdpProxyTable,
    destination: InternetAddr,
}

impl loading::Hook for UdpProxyAccess {}

impl UdpProxyAccess {
    pub fn new(proxy_table: UdpProxyTable, destination: InternetAddr) -> Self {
        Self {
            proxy_table,
            destination,
        }
    }

    pub async fn build(self, listen_addr: impl ToSocketAddrs) -> io::Result<UdpServer<Self>> {
        let listener = tokio::net::UdpSocket::bind(listen_addr)
            .await
            .inspect_err(|e| error!(?e, "Failed to bind to listen address"))?;
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
            UdpProxySocket::establish(proxy_chain.chain.clone(), self.destination.clone()).await?;

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
impl UdpServerHook for UdpProxyAccess {
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
