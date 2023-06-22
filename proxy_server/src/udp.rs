use std::{
    io::{self, Write},
    net::SocketAddr,
    sync::Arc,
};

use async_trait::async_trait;
use common::{
    addr::{any_addr, InternetAddr},
    crypto::{XorCrypto, XorCryptoCursor},
    header::{
        codec::{read_header, write_header, CodecError},
        route::{RouteError, RouteErrorKind, RouteResponse},
    },
    loading,
    udp::{
        header::UdpRequestHeader, Flow, FlowMetrics, Packet, UdpDownstreamWriter, UdpServer,
        UdpServerHook, UpstreamAddr, BUFFER_LENGTH, LIVE_CHECK_INTERVAL, TIMEOUT,
    },
};
use serde::Deserialize;
use thiserror::Error;
use tokio::{
    net::{ToSocketAddrs, UdpSocket},
    sync::mpsc,
};
use tracing::{error, info, instrument, trace, warn};

#[derive(Debug, Clone, PartialEq, Eq, Hash, Deserialize)]
pub struct UdpProxyServerBuilder {
    pub listen_addr: Arc<str>,
    pub header_xor_key: Arc<[u8]>,
}

#[async_trait]
impl loading::Builder for UdpProxyServerBuilder {
    type Hook = UdpProxyServer;
    type Server = UdpServer<Self::Hook>;

    async fn build_server(self) -> io::Result<Self::Server> {
        let header_crypto = XorCrypto::new(self.header_xor_key);
        let tcp_proxy = UdpProxyServer::new(header_crypto);
        let server = tcp_proxy.build(self.listen_addr.as_ref()).await?;
        Ok(server)
    }

    fn build_hook(self) -> io::Result<Self::Hook> {
        let header_crypto = XorCrypto::new(self.header_xor_key);
        Ok(UdpProxyServer::new(header_crypto))
    }

    fn key(&self) -> &Arc<str> {
        &self.listen_addr
    }
}

impl UdpProxyServerBuilder {
    pub async fn build(self) -> io::Result<UdpServer<UdpProxyServer>> {
        let header_crypto = XorCrypto::new(self.header_xor_key);
        let tcp_proxy = UdpProxyServer::new(header_crypto);
        let server = tcp_proxy.build(self.listen_addr.as_ref()).await?;
        Ok(server)
    }
}

#[derive(Debug)]
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
    ) -> Result<(Option<UpstreamAddr>, &'buf [u8]), CodecError> {
        // Decode header
        let mut reader = io::Cursor::new(buf);
        let mut crypto_cursor = XorCryptoCursor::new(&self.header_crypto);
        let header: UdpRequestHeader = read_header(&mut reader, &mut crypto_cursor)?;
        let header_len = reader.position() as usize;
        let payload = &buf[header_len..];

        Ok((header.upstream.map(UpstreamAddr), payload))
    }

    async fn handle_steer_error(&self, downstream_writer: &UdpDownstreamWriter, error: CodecError) {
        let peer_addr = downstream_writer.peer_addr();
        warn!(?error, ?peer_addr, "Failed to steer");
        let kind = error_kind_from_header_error(error);
        if let Err(e) = self.respond_with_error(downstream_writer, kind).await {
            trace!(?e, ?peer_addr, "Failed to respond with error to downstream");
        }
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
        let mut downlink_buf = [0; BUFFER_LENGTH];
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
                    upstream.send(&packet.0).await.map_err(|e| ProxyError::ForwardUpstream { source: e, addr: flow.upstream.0.clone(), sock_addr: resolved_upstream })?;
                    bytes_uplink += packet.0.len() as u64;
                    packets_uplink += 1;

                    last_packet = std::time::Instant::now();
                }
                res = upstream.recv(&mut downlink_buf) => {
                    trace!("Received packet from upstream");
                    let n = res.map_err(|e| ProxyError::RecvUpstream { source: e, addr: flow.upstream.0.clone(), sock_addr: resolved_upstream })?;
                    let pkt = &downlink_buf[..n];

                    // Set up protocol buffer writer
                    downlink_protocol_buf.clear();
                    let mut writer = io::Cursor::new(&mut downlink_protocol_buf);

                    // Write header
                    let header = RouteResponse {
                        result: Ok(()),
                    };
                    let mut crypto_cursor = XorCryptoCursor::new(&self.header_crypto);
                    write_header(&mut writer, &header, &mut crypto_cursor).unwrap();

                    // Write payload
                    writer.write_all(pkt).unwrap();

                    // Send packet to downstream
                    let pos = writer.position() as usize;
                    let pkt = &downlink_protocol_buf[..pos];
                    downstream_writer.send(pkt).await.map_err(|e| ProxyError::ForwardDownstream { source: e, downstream: downstream_writer.clone() })?;
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

    async fn handle_proxy_result(
        &self,
        downstream_writer: &UdpDownstreamWriter,
        res: Result<FlowMetrics, ProxyError>,
    ) {
        match res {
            Ok(metrics) => {
                info!(%metrics, "Proxy finished");
                // No response
            }
            Err(e) => {
                let peer_addr = downstream_writer.peer_addr();
                warn!(?e, ?peer_addr, "Proxy failed");
                let kind = error_kind_from_proxy_error(e);
                if let Err(e) = self.respond_with_error(downstream_writer, kind).await {
                    trace!(?e, ?peer_addr, "Failed to respond with error");
                }
            }
        }
    }

    #[instrument(skip(self, downstream_writer))]
    async fn respond_with_error(
        &self,
        downstream_writer: &UdpDownstreamWriter,
        kind: RouteErrorKind,
    ) -> Result<(), io::Error> {
        // Respond with error
        let resp = RouteResponse {
            result: Err(RouteError { kind }),
        };
        let mut buf = Vec::new();
        let mut crypto_cursor = XorCryptoCursor::new(&self.header_crypto);
        write_header(&mut buf, &resp, &mut crypto_cursor).unwrap();
        downstream_writer.send(&buf).await.map_err(|e| {
            let peer_addr = downstream_writer.peer_addr();
            trace!(?e, ?peer_addr, "Failed to send response to downstream");
            e
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

fn error_kind_from_header_error(e: CodecError) -> RouteErrorKind {
    match e {
        CodecError::Io(_) => RouteErrorKind::Io,
        CodecError::Bincode(_) => RouteErrorKind::Codec,
    }
}
fn error_kind_from_proxy_error(e: ProxyError) -> RouteErrorKind {
    match e {
        ProxyError::Resolve { .. }
        | ProxyError::ClientBindAny(_)
        | ProxyError::ConnectUpstream { .. }
        | ProxyError::ForwardUpstream { .. }
        | ProxyError::RecvUpstream { .. }
        | ProxyError::ForwardDownstream { .. } => RouteErrorKind::Io,
        ProxyError::Loopback { .. } => RouteErrorKind::Loopback,
    }
}

impl loading::Hook for UdpProxyServer {}

#[async_trait]
impl UdpServerHook for UdpProxyServer {
    async fn parse_upstream_addr<'buf>(
        &self,
        buf: &'buf [u8],
        downstream_writer: &UdpDownstreamWriter,
    ) -> Option<(UpstreamAddr, &'buf [u8])> {
        let res = self.steer(buf).await;
        match res {
            Ok((upstream_addr, payload)) => {
                // Proxy
                if let Some(addr) = upstream_addr {
                    return Some((addr, payload));
                }

                // Echo
                let resp = RouteResponse { result: Ok(()) };
                let mut wtr = Vec::new();
                let mut crypto_cursor = XorCryptoCursor::new(&self.header_crypto);
                write_header(&mut wtr, &resp, &mut crypto_cursor).unwrap();
                wtr.write_all(payload).unwrap();
                let downstream_writer = downstream_writer.clone();
                tokio::spawn(async move {
                    if let Err(e) = downstream_writer.send(&wtr).await {
                        warn!(
                            ?e,
                            ?downstream_writer,
                            "Failed to send response to downstream"
                        );
                    };
                });
                None
            }
            Err(err) => {
                self.handle_steer_error(downstream_writer, err).await;
                None
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
