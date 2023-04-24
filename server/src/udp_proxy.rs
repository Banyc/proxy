use std::{
    io::{self, Write},
    time::Duration,
};

use async_trait::async_trait;
use common::{
    addr::any_addr,
    error::{ProxyProtocolError, ResponseError, ResponseErrorKind},
    header::{read_header, write_header, RequestHeader, ResponseHeader, XorCrypto},
    udp::{Flow, Packet, UdpDownstreamWriter, UdpServer, UdpServerHook, UpstreamAddr},
};
use tokio::{
    net::{ToSocketAddrs, UdpSocket},
    sync::mpsc,
};
use tracing::{error, info, instrument, trace};

pub struct UdpProxy {
    crypto: XorCrypto,
}

impl UdpProxy {
    pub fn new(crypto: XorCrypto) -> Self {
        Self { crypto }
    }

    pub async fn build(self, listen_addr: impl ToSocketAddrs) -> io::Result<UdpServer<Self>> {
        let listener = UdpSocket::bind(listen_addr).await?;
        Ok(UdpServer::new(listener, self))
    }

    #[instrument(skip(self, buf))]
    async fn steer<'buf>(
        &self,
        buf: &'buf [u8],
    ) -> Result<(UpstreamAddr, &'buf [u8]), ProxyProtocolError> {
        // Decode header
        let mut reader = io::Cursor::new(buf);
        let header: RequestHeader = read_header(&mut reader, &self.crypto)
            .inspect_err(|e| error!(?e, "Failed to decode header from downstream"))?;
        let header_len = reader.position() as usize;
        let payload = &buf[header_len..];

        // Prevent connections to localhost
        let upstream = header.upstream.to_socket_addr().await?;
        if upstream.ip().is_loopback() {
            error!(?header, "Loopback address is not allowed");
            return Err(ProxyProtocolError::Loopback);
        }

        Ok((UpstreamAddr(upstream), payload))
    }

    async fn handle_steer_error(
        &self,
        downstream_writer: &UdpDownstreamWriter,
        error: ProxyProtocolError,
    ) {
        error!(?error, "Failed to steer");
        let _ = self
            .respond_with_error(downstream_writer, error)
            .await
            .inspect_err(|e| error!(?e, "Failed to respond with error to downstream"));
    }

    #[instrument(skip(self, rx, downstream_writer))]
    async fn proxy(
        &self,
        mut rx: mpsc::Receiver<Packet>,
        flow: Flow,
        downstream_writer: UdpDownstreamWriter,
    ) -> Result<FlowMetrics, ProxyProtocolError> {
        let start = std::time::Instant::now();

        // Connect to upstream
        let any_addr = any_addr(&flow.upstream.0.ip());
        let upstream = UdpSocket::bind(any_addr).await?;
        upstream.connect(flow.upstream.0).await?;

        // Periodic check if the flow is still alive
        let mut tick = tokio::time::interval(LIVE_CHECK_INTERVAL);
        let mut last_packet = std::time::Instant::now();

        let mut bytes_uplink = 0;
        let mut bytes_downlink = 0;
        let mut packets_uplink = 0;
        let mut packets_downlink = 0;

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
                    bytes_uplink += &packet.0.len();
                    packets_uplink += 1;

                    last_packet = std::time::Instant::now();
                }
                res = upstream.recv(&mut downlink_buf) => {
                    trace!("Received packet from upstream");
                    let n = res?;
                    let pkt = &downlink_buf[..n];

                    // Write header
                    let mut writer = io::Cursor::new(Vec::new());
                    let header = ResponseHeader {
                        result: Ok(()),
                    };
                    write_header(&mut writer, &header, &self.crypto)?;

                    // Write payload
                    writer.write_all(pkt)?;

                    // Send packet to downstream
                    let pkt = writer.into_inner();
                    downstream_writer.send(&pkt).await?;
                    bytes_downlink += &pkt.len();
                    packets_downlink += 1;

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

    #[instrument(skip(self, downstream_writer, res))]
    async fn handle_proxy_result(
        &self,
        downstream_writer: &UdpDownstreamWriter,
        res: Result<FlowMetrics, ProxyProtocolError>,
    ) {
        match res {
            Ok(metrics) => {
                info!(?metrics, "Connection closed normally");
                // No response
            }
            Err(e) => {
                error!(?e, "Connection closed with error");
                let _ = self
                    .respond_with_error(downstream_writer, e)
                    .await
                    .inspect_err(|e| error!(?e, "Failed to respond with error"));
            }
        }
    }

    #[instrument(skip(self, downstream_writer))]
    async fn respond_with_error(
        &self,
        downstream_writer: &UdpDownstreamWriter,
        error: ProxyProtocolError,
    ) -> Result<(), ProxyProtocolError> {
        let local_addr = downstream_writer
            .local_addr()
            .inspect_err(|e| error!(?e, "Failed to get local address"))?;

        // Respond with error
        let resp = match error {
            ProxyProtocolError::Io(_) => ResponseHeader {
                result: Err(ResponseError {
                    source: local_addr.into(),
                    kind: ResponseErrorKind::Io,
                }),
            },
            ProxyProtocolError::Bincode(_) => ResponseHeader {
                result: Err(ResponseError {
                    source: local_addr.into(),
                    kind: ResponseErrorKind::Codec,
                }),
            },
            ProxyProtocolError::Loopback => ResponseHeader {
                result: Err(ResponseError {
                    source: local_addr.into(),
                    kind: ResponseErrorKind::Loopback,
                }),
            },
            ProxyProtocolError::Response(err) => ResponseHeader { result: Err(err) },
        };
        let mut buf = Vec::new();
        write_header(&mut buf, &resp, &self.crypto).unwrap();
        downstream_writer
            .send(&buf)
            .await
            .inspect_err(|e| error!(?e, "Failed to send response to downstream"))?;

        Ok(())
    }
}

#[async_trait]
impl UdpServerHook for UdpProxy {
    async fn parse_upstream_addr<'buf>(
        &self,
        buf: &'buf [u8],
        downstream_writer: &UdpDownstreamWriter,
    ) -> Result<(UpstreamAddr, &'buf [u8]), ()> {
        let res = self.steer(buf).await;
        match res {
            Ok((upstream_addr, payload)) => Ok((upstream_addr, payload)),
            Err(err) => {
                self.handle_steer_error(downstream_writer, err).await;
                Err(())
            }
        }
    }

    async fn handle_flow(
        &self,
        rx: mpsc::Receiver<Packet>,
        flow: Flow,
        downstream_writer: UdpDownstreamWriter,
    ) {
        let res = self.proxy(rx, flow, downstream_writer.clone()).await;
        self.handle_proxy_result(&downstream_writer, res).await;
    }
}

const TIMEOUT: Duration = Duration::from_secs(10);
const LIVE_CHECK_INTERVAL: Duration = Duration::from_secs(1);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct FlowMetrics {
    flow: Flow,
    start: std::time::Instant,
    end: std::time::Instant,
    bytes_uplink: usize,
    bytes_downlink: usize,
    packets_uplink: usize,
    packets_downlink: usize,
}
