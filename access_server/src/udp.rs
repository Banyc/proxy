use std::{
    io::{self, Write},
    net::Ipv4Addr,
    time::Duration,
};

use async_trait::async_trait;
use common::{
    addr::any_addr,
    addr::InternetAddr,
    udp::{
        config::{UdpProxyTable, UdpProxyTableBuilder},
        Flow, Packet, UdpDownstreamWriter, UdpServer, UdpServerHook, UpstreamAddr,
    },
};
use proxy_client::udp::{EstablishError, RecvError, SendError, UdpProxySocket};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::{net::ToSocketAddrs, sync::mpsc};
use tracing::{error, info, trace};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UdpProxyAccessBuilder {
    listen_addr: String,
    proxy_table: UdpProxyTableBuilder,
    destination: String,
}

impl UdpProxyAccessBuilder {
    pub async fn build(self) -> io::Result<UdpServer<UdpProxyAccess>> {
        let access = UdpProxyAccess::new(self.proxy_table.build(), self.destination.into());
        let server = access.build(self.listen_addr).await?;
        Ok(server)
    }
}

pub struct UdpProxyAccess {
    proxy_table: UdpProxyTable,
    destination: InternetAddr,
}

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
    ) -> Result<(), ProxyError> {
        // Connect to upstream
        let proxy_chain = self.proxy_table.choose_chain();
        let upstream =
            UdpProxySocket::establish(proxy_chain.chain.clone(), self.destination.clone()).await?;

        // Periodic check if the flow is still alive
        let mut tick = tokio::time::interval(LIVE_CHECK_INTERVAL);
        let mut last_packet = std::time::Instant::now();

        // Forward packets
        let mut downlink_buf = [0; 1024];
        let mut downlink_protocol_buf = Vec::new();
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

                    last_packet = std::time::Instant::now();
                }
                res = upstream.recv(&mut downlink_buf) => {
                    trace!("Received packet from upstream");
                    let n = res?;
                    let pkt = &downlink_buf[..n];

                    // Set up protocol buffer writer
                    downlink_protocol_buf.clear();
                    let mut writer = io::Cursor::new(&mut downlink_protocol_buf);

                    // Write payload
                    writer.write_all(pkt).unwrap();

                    // Send packet to downstream
                    let pkt = writer.into_inner();
                    downstream_writer.send(pkt).await.map_err(|e| ProxyError::SendDownstream { source: e, downstream: downstream_writer.clone() })?;

                    last_packet = std::time::Instant::now();
                }
                _ = tick.tick() => {
                    trace!("Checking if flow is still alive");
                    if last_packet.elapsed() > TIMEOUT {
                        info!(?flow, "Flow timed out");
                        break;
                    }
                }
            }
        }

        Ok(())
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
    ) -> Result<(UpstreamAddr, &'buf [u8]), ()> {
        Ok((
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
        if let Err(e) = res {
            error!(?e, "Failed to proxy");
        }
    }
}

const TIMEOUT: Duration = Duration::from_secs(10);
const LIVE_CHECK_INTERVAL: Duration = Duration::from_secs(1);
