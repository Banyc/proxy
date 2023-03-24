use std::{
    io::{self, Write},
    net::{Ipv4Addr, SocketAddr},
    time::Duration,
};

use async_trait::async_trait;
use client::udp_proxy_client::UdpProxySocket;
use common::{
    addr::any_addr,
    error::ProxyProtocolError,
    header::ProxyConfig,
    udp::{Flow, Packet, UdpDownstreamWriter, UdpServer, UdpServerHook, UpstreamAddr},
};
use tokio::{net::ToSocketAddrs, sync::mpsc};
use tracing::{error, info, trace};

pub struct UdpProxyAccess {
    proxy_configs: Vec<ProxyConfig>,
    destination: SocketAddr,
}

impl UdpProxyAccess {
    pub fn new(proxy_configs: Vec<ProxyConfig>, destination: SocketAddr) -> Self {
        Self {
            proxy_configs,
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
    ) -> Result<(), ProxyProtocolError> {
        // Connect to upstream
        let upstream =
            UdpProxySocket::establish(self.proxy_configs.clone(), &self.destination).await?;

        // Periodic check if the flow is still alive
        let mut tick = tokio::time::interval(LIVE_CHECK_INTERVAL);
        let mut last_packet = std::time::Instant::now();

        // Forward packets
        let mut downlink_buf = [0; 1024];
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

                    // Write payload
                    let mut writer = io::Cursor::new(Vec::new());
                    writer.write_all(pkt)?;

                    // Send packet to downstream
                    let pkt = writer.into_inner();
                    downstream_writer.send_to(&pkt, flow.downstream.0).await?;

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

#[async_trait]
impl UdpServerHook for UdpProxyAccess {
    async fn parse_upstream_addr<'buf>(
        &self,
        buf: &'buf [u8],
        _downstream_writer: &UdpDownstreamWriter,
    ) -> Result<(UpstreamAddr, &'buf [u8]), ()> {
        Ok((UpstreamAddr(any_addr(&Ipv4Addr::UNSPECIFIED.into())), buf))
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
