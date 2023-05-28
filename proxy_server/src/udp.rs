use std::{
    io::{self, Write},
    net::SocketAddr,
    time::Duration,
};

use async_trait::async_trait;
use common::{
    addr::{any_addr, InternetAddr},
    crypto::{XorCrypto, XorCryptoCursor},
    error::{ResponseError, ResponseErrorKind},
    header::{read_header, write_header, HeaderError, ResponseHeader},
    udp::{
        header::UdpRequestHeader, Flow, Packet, UdpDownstreamWriter, UdpServer, UdpServerHook,
        UpstreamAddr,
    },
};
use serde::Deserialize;
use thiserror::Error;
use tokio::{
    net::{ToSocketAddrs, UdpSocket},
    sync::mpsc,
};
use tracing::{error, info, instrument, trace};

#[derive(Debug, Clone, PartialEq, Eq, Hash, Deserialize)]
pub struct UdpProxyServerBuilder {
    pub listen_addr: String,
    pub header_xor_key: Vec<u8>,
}

impl UdpProxyServerBuilder {
    pub async fn build(self) -> io::Result<UdpServer<UdpProxyServer>> {
        let header_crypto = XorCrypto::new(self.header_xor_key);
        let tcp_proxy = UdpProxyServer::new(header_crypto);
        let server = tcp_proxy.build(self.listen_addr).await?;
        Ok(server)
    }
}

pub struct UdpProxyServer {
    header_crypto: XorCrypto,
}

impl UdpProxyServer {
    pub fn new(header_crypto: XorCrypto) -> Self {
        Self { header_crypto }
    }

    pub async fn build(self, listen_addr: impl ToSocketAddrs) -> io::Result<UdpServer<Self>> {
        let listener = UdpSocket::bind(listen_addr).await?;
        Ok(UdpServer::new(listener, self))
    }

    #[instrument(skip(self, buf))]
    async fn steer<'buf>(
        &self,
        buf: &'buf [u8],
    ) -> Result<(UpstreamAddr, &'buf [u8]), HeaderError> {
        // Decode header
        let mut reader = io::Cursor::new(buf);
        let mut crypto_cursor = XorCryptoCursor::new(&self.header_crypto);
        let header: UdpRequestHeader = read_header(&mut reader, &mut crypto_cursor)?;
        let header_len = reader.position() as usize;
        let payload = &buf[header_len..];

        Ok((UpstreamAddr(header.upstream), payload))
    }

    async fn handle_steer_error(
        &self,
        downstream_writer: &UdpDownstreamWriter,
        error: HeaderError,
    ) {
        let peer_addr = downstream_writer.remote_addr();
        error!(?error, ?peer_addr, "Failed to steer");
        let kind = error_kind_from_header_error(error);
        let _ = self
            .respond_with_error(downstream_writer, kind)
            .await
            .inspect_err(|e| trace!(?e, ?peer_addr, "Failed to respond with error to downstream"));
    }

    #[instrument(skip(self, rx, downstream_writer))]
    async fn proxy(
        &self,
        mut rx: mpsc::Receiver<Packet>,
        flow: Flow,
        downstream_writer: UdpDownstreamWriter,
    ) -> Result<FlowMetrics, ProxyError> {
        let start = std::time::Instant::now();

        // Prevent connections to localhost
        let resolved_upstream =
            flow.upstream
                .0
                .to_socket_addr()
                .await
                .map_err(|e| ProxyError::Resolve {
                    source: e,
                    addr: flow.upstream.0.clone(),
                })?;
        if resolved_upstream.ip().is_loopback() {
            return Err(ProxyError::Loopback {
                addr: flow.upstream.0,
                sock_addr: resolved_upstream,
            });
        }

        // Connect to upstream
        let any_addr = any_addr(&resolved_upstream.ip());
        let upstream = UdpSocket::bind(any_addr)
            .await
            .map_err(ProxyError::ClientBindAny)?;
        upstream
            .connect(resolved_upstream)
            .await
            .map_err(|e| ProxyError::ConnectUpstream {
                source: e,
                addr: flow.upstream.0.clone(),
                sock_addr: resolved_upstream,
            })?;

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
                    upstream.send(&packet.0).await.map_err(|e| ProxyError::ForwardUpstream {
                        source: e,
                        addr: flow.upstream.0.clone(),
                sock_addr: resolved_upstream,
                    })?;
                    bytes_uplink += &packet.0.len();
                    packets_uplink += 1;

                    last_packet = std::time::Instant::now();
                }
                res = upstream.recv(&mut downlink_buf) => {
                    trace!("Received packet from upstream");
                    let n = res.map_err(|e| ProxyError::RecvUpstream {
                        source: e,
                        addr: flow.upstream.0.clone(),
                sock_addr: resolved_upstream,
                    })?;
                    let pkt = &downlink_buf[..n];

                    // Write header
                    let mut writer = io::Cursor::new(Vec::new());
                    let header = ResponseHeader {
                        result: Ok(()),
                    };
                    let mut crypto_cursor = XorCryptoCursor::new(&self.header_crypto);
                    write_header(&mut writer, &header, &mut crypto_cursor).unwrap();

                    // Write payload
                    writer.write_all(pkt).unwrap();

                    // Send packet to downstream
                    let pkt = writer.into_inner();
                    downstream_writer.send(&pkt).await.map_err(|e| ProxyError::ForwardDownstream {
                        source: e, downstream: downstream_writer.clone(),
                    })?;
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
        res: Result<FlowMetrics, ProxyError>,
    ) {
        match res {
            Ok(metrics) => {
                info!(?metrics, "Proxy finished");
                // No response
            }
            Err(e) => {
                let peer_addr = downstream_writer.remote_addr();
                error!(?e, ?peer_addr, "Proxy failed");
                let kind = error_kind_from_proxy_error(e);
                let _ = self
                    .respond_with_error(downstream_writer, kind)
                    .await
                    .inspect_err(|e| trace!(?e, ?peer_addr, "Failed to respond with error"));
            }
        }
    }

    #[instrument(skip(self, downstream_writer))]
    async fn respond_with_error(
        &self,
        downstream_writer: &UdpDownstreamWriter,
        kind: ResponseErrorKind,
    ) -> Result<(), io::Error> {
        let local_addr = downstream_writer
            .local_addr()
            .inspect_err(|e| error!(?e, "Failed to get local address"))?;

        // Respond with error
        let resp = ResponseHeader {
            result: Err(ResponseError {
                source: local_addr.into(),
                kind,
            }),
        };
        let mut buf = Vec::new();
        let mut crypto_cursor = XorCryptoCursor::new(&self.header_crypto);
        write_header(&mut buf, &resp, &mut crypto_cursor).unwrap();
        downstream_writer.send(&buf).await.inspect_err(|e| {
            let peer_addr = downstream_writer.remote_addr();
            trace!(?e, ?peer_addr, "Failed to send response to downstream")
        })?;

        Ok(())
    }
}

#[derive(Debug, Error)]
pub enum ProxyError {
    #[error("Failed to resolve upstream address")]
    Resolve {
        #[source]
        source: io::Error,
        addr: InternetAddr,
    },
    #[error("Header error")]
    Header(#[from] HeaderError),
    #[error("Refused to connect to a loopback address")]
    Loopback {
        addr: InternetAddr,
        sock_addr: SocketAddr,
    },
    #[error("Failed to created a client socket")]
    ClientBindAny(#[source] io::Error),
    #[error("Failed to connect to upstream")]
    ConnectUpstream {
        #[source]
        source: io::Error,
        addr: InternetAddr,
        sock_addr: SocketAddr,
    },
    #[error("Failed to forward packet from downstream to upstream")]
    ForwardUpstream {
        #[source]
        source: io::Error,
        addr: InternetAddr,
        sock_addr: SocketAddr,
    },
    #[error("Failed to recv from upstream")]
    RecvUpstream {
        #[source]
        source: io::Error,
        addr: InternetAddr,
        sock_addr: SocketAddr,
    },
    #[error("Failed to forward packet from upstream to downstream")]
    ForwardDownstream {
        #[source]
        source: io::Error,
        downstream: UdpDownstreamWriter,
    },
}

fn error_kind_from_header_error(e: HeaderError) -> ResponseErrorKind {
    match e {
        HeaderError::Io(_) => ResponseErrorKind::Io,
        HeaderError::Bincode(_) => ResponseErrorKind::Codec,
    }
}
fn error_kind_from_proxy_error(e: ProxyError) -> ResponseErrorKind {
    match e {
        ProxyError::Resolve { .. }
        | ProxyError::ClientBindAny(_)
        | ProxyError::ConnectUpstream { .. }
        | ProxyError::ForwardUpstream { .. }
        | ProxyError::RecvUpstream { .. }
        | ProxyError::ForwardDownstream { .. } => ResponseErrorKind::Io,
        ProxyError::Header(e) => error_kind_from_header_error(e),
        ProxyError::Loopback { .. } => ResponseErrorKind::Loopback,
    }
}

#[async_trait]
impl UdpServerHook for UdpProxyServer {
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

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct FlowMetrics {
    flow: Flow,
    start: std::time::Instant,
    end: std::time::Instant,
    bytes_uplink: usize,
    bytes_downlink: usize,
    packets_uplink: usize,
    packets_downlink: usize,
}
