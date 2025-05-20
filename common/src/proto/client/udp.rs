use std::{
    io::{self, Write},
    net::SocketAddr,
    num::NonZeroUsize,
    sync::Arc,
    time::{Duration, Instant},
};

use crate::{
    addr::InternetAddr,
    anti_replay::{TimeValidator, VALIDATOR_TIME_FRAME, VALIDATOR_UDP_HDR_TTL, ValidatorRef},
    error::AnyError,
    header::{
        codec::{CodecError, MAX_HEADER_LEN, read_header, write_header},
        route::{RouteError, RouteResponse},
    },
    proto::{
        context::UdpContext,
        io_copy::udp::{UdpRecv, UdpSend},
        route::UdpConnChain,
    },
    route::{BuildTracer, TraceRtt, convert_proxies_to_header_crypto_pairs},
    ttl_cell::TtlCell,
    udp::PACKET_BUFFER_LENGTH,
};
use bytes::BytesMut;
use metrics::counter;
use primitive::arena::obj_pool::ArcObjPool;
use thiserror::Error;
use tokio::net::UdpSocket;
use tracing::{error, instrument, trace, warn};

#[derive(Debug)]
pub struct UdpProxyClient {
    write: UdpProxyClientWriteHalf,
    read: UdpProxyClientReadHalf,
    upstream_addr: InternetAddr,
}
impl UdpProxyClient {
    #[instrument(skip_all)]
    pub async fn establish(
        proxies: Arc<UdpConnChain>,
        destination: InternetAddr,
        context: &UdpContext,
    ) -> Result<UdpProxyClient, EstablishError> {
        // If there are no proxy configs, just connect to the destination
        if proxies.is_empty() {
            let addr = destination.to_socket_addr().await.map_err(|e| {
                EstablishError::ResolveDestination {
                    source: e,
                    addr: destination.clone(),
                }
            })?;
            let upstream = context.connector.connect(addr).await.map_err(|e| {
                EstablishError::ConnectDestination {
                    source: e,
                    addr: destination.clone(),
                    sock_addr: addr,
                }
            })?;

            let upstream = Arc::new(upstream);
            let write = UdpProxyClientWriteHalf::new(upstream.clone(), None);
            let read = UdpProxyClientReadHalf::new(upstream, 0, proxies);
            return Ok(UdpProxyClient {
                write,
                read,
                upstream_addr: destination,
            });
        }

        // Connect to upstream
        let proxy_addr = proxies[0].address.clone();
        let addr =
            proxy_addr
                .to_socket_addr()
                .await
                .map_err(|e| EstablishError::ResolveFirstProxy {
                    source: e,
                    addr: proxy_addr.clone(),
                })?;
        let upstream = context.connector.connect(addr).await.map_err(|e| {
            EstablishError::ConnectFirstProxy {
                source: e,
                addr: proxy_addr.clone(),
                sock_addr: addr,
            }
        })?;

        // Convert addresses to headers
        let pairs = convert_proxies_to_header_crypto_pairs(&proxies, Some(destination));

        // Save headers to buffer
        let request_header = {
            let pairs = pairs
                .into_iter()
                .map(|(header, crypto)| (header, crypto.clone()))
                .collect::<Vec<_>>();
            move || {
                let mut buf = Vec::new();
                let mut writer = io::Cursor::new(&mut buf);
                for (header, crypto) in &pairs {
                    trace!(?header, "Writing header to buffer");
                    let mut crypto_cursor =
                        tokio_chacha20::cursor::EncryptCursor::new_x(*crypto.key());
                    write_header(&mut writer, header, &mut crypto_cursor).unwrap();
                }
                buf.into()
            }
        };

        // Return stream
        let upstream = Arc::new(upstream);
        let write = UdpProxyClientWriteHalf::new(upstream.clone(), Some(Box::new(request_header)));
        let read = UdpProxyClientReadHalf::new(upstream, MAX_HEADER_LEN, proxies);
        Ok(UdpProxyClient {
            write,
            read,
            upstream_addr: proxy_addr,
        })
    }

    pub fn into_split(self) -> (UdpProxyClientReadHalf, UdpProxyClientWriteHalf) {
        (self.read, self.write)
    }

    pub fn remote_addr(&self) -> &InternetAddr {
        &self.upstream_addr
    }
}
#[derive(Debug, Error)]
pub enum EstablishError {
    #[error("Failed to resolve destination address: {source}, {addr}")]
    ResolveDestination {
        #[source]
        source: io::Error,
        addr: InternetAddr,
    },
    #[error("Failed to connect to destination: {source}, {addr}, {sock_addr}")]
    ConnectDestination {
        #[source]
        source: io::Error,
        addr: InternetAddr,
        sock_addr: SocketAddr,
    },
    #[error("Failed to resolve first proxy address: {source}, {addr}")]
    ResolveFirstProxy {
        #[source]
        source: io::Error,
        addr: InternetAddr,
    },
    #[error("Failed to connect to first proxy: {source}, {addr}, {sock_addr}")]
    ConnectFirstProxy {
        #[source]
        source: io::Error,
        addr: InternetAddr,
        sock_addr: SocketAddr,
    },
}

pub struct UdpProxyClientWriteHalf {
    upstream: Arc<UdpSocket>,
    request_header: Option<Box<dyn Fn() -> Arc<[u8]> + Send>>,
    request_header_ttl: TtlCell<Arc<[u8]>>,
    write_buf: Vec<u8>,
}
impl core::fmt::Debug for UdpProxyClientWriteHalf {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("UdpProxyClientWriteHalf")
            .field("upstream", &self.upstream)
            .field("write_buf", &self.write_buf)
            .finish()
    }
}
impl UdpSend for UdpProxyClientWriteHalf {
    async fn trait_send(&mut self, buf: &[u8]) -> Result<usize, AnyError> {
        Self::send(self, buf).await.map_err(|e| e.into())
    }
}
impl UdpProxyClientWriteHalf {
    pub fn new(
        upstream: Arc<UdpSocket>,
        request_header: Option<Box<dyn Fn() -> Arc<[u8]> + Send>>,
    ) -> Self {
        Self {
            upstream,
            request_header,
            write_buf: vec![],
            request_header_ttl: TtlCell::new(None, VALIDATOR_UDP_HDR_TTL),
        }
    }

    #[instrument(skip_all)]
    pub async fn send(&mut self, buf: &[u8]) -> Result<usize, SendError> {
        self.write_buf.clear();

        if let Some(request_header) = &self.request_header {
            let hdr = match self.request_header_ttl.get() {
                Some(hdr) => hdr,
                None => self.request_header_ttl.set(request_header()),
            };
            // Write header
            self.write_buf.write_all(hdr).unwrap();
        }

        // Write payload
        self.write_buf.write_all(buf).unwrap();

        // Send data
        self.upstream.send(&self.write_buf).await.map_err(|e| {
            let peer_addr = self.upstream.peer_addr().ok();
            SendError {
                source: e,
                sock_addr: peer_addr,
            }
        })?;

        Ok(buf.len())
    }

    pub fn inner(&self) -> &Arc<UdpSocket> {
        &self.upstream
    }
}
#[derive(Debug, Error)]
#[error("Failed to send to upstream: {source}, {sock_addr:?}")]
pub struct SendError {
    #[source]
    source: io::Error,
    sock_addr: Option<SocketAddr>,
}

fn time_validator() -> TimeValidator {
    TimeValidator::new(VALIDATOR_TIME_FRAME + VALIDATOR_UDP_HDR_TTL)
}

#[derive(Debug)]
pub struct UdpProxyClientReadHalf {
    upstream: Arc<UdpSocket>,
    max_response_header_size: usize,
    proxies: Arc<UdpConnChain>,
    read_buf: Vec<u8>,
    time_validator: TimeValidator,
}
impl UdpRecv for UdpProxyClientReadHalf {
    async fn trait_recv(&mut self, buf: &mut [u8]) -> Result<usize, AnyError> {
        Self::recv(self, buf).await.map_err(|e| e.into())
    }
}
impl UdpProxyClientReadHalf {
    pub fn new(
        upstream: Arc<UdpSocket>,
        max_response_header_size: usize,
        proxies: Arc<UdpConnChain>,
    ) -> Self {
        Self {
            upstream,
            max_response_header_size,
            proxies,
            read_buf: vec![],
            time_validator: time_validator(),
        }
    }

    #[instrument(skip_all)]
    pub async fn recv(&mut self, buf: &mut [u8]) -> Result<usize, RecvError> {
        let cap = self.max_response_header_size + buf.len();
        self.read_buf.resize(cap, 0);

        // Read data
        let n = self.upstream.recv(&mut self.read_buf).await.map_err(|e| {
            let peer_addr = self.upstream.peer_addr().ok();
            RecvError::RecvUpstream {
                source: e,
                sock_addr: peer_addr,
            }
        })?;
        let mut reader = io::Cursor::new(&self.read_buf[..n]);

        // Decode and check headers
        for node in self.proxies.iter() {
            trace!(?node.address, "Reading response");
            let mut crypto_cursor =
                tokio_chacha20::cursor::DecryptCursor::new_x(*node.header_crypto.key());
            let validator = ValidatorRef::Time(&self.time_validator);
            let resp: RouteResponse = read_header(&mut reader, &mut crypto_cursor, &validator)?;
            if let Err(err) = resp.result {
                warn!(?err, %node.address, "Upstream responded with an error");
                return Err(RecvError::Response {
                    err,
                    addr: node.address.clone(),
                });
            }
        }

        // Read payload
        let payload_size = reader.get_ref().len() - reader.position() as usize;
        buf[..payload_size].copy_from_slice(&reader.get_ref()[reader.position() as usize..]);

        Ok(payload_size)
    }

    pub fn inner(&self) -> &Arc<UdpSocket> {
        &self.upstream
    }
}
#[derive(Debug, Error)]
pub enum RecvError {
    #[error("Failed to recv from upstream: {source}, {sock_addr:?}")]
    RecvUpstream {
        #[source]
        source: io::Error,
        sock_addr: Option<SocketAddr>,
    },
    #[error("Failed to read response from upstream: {0}")]
    Header(#[from] CodecError),
    #[error("Upstream responded with an error: {err}, {addr}")]
    Response { err: RouteError, addr: InternetAddr },
}

pub struct UdpTracerBuilder {
    context: UdpContext,
}
impl UdpTracerBuilder {
    pub fn new(context: UdpContext) -> Self {
        Self { context }
    }
}
impl BuildTracer for UdpTracerBuilder {
    type Tracer = UdpTracer;

    fn build(&self) -> Self::Tracer {
        UdpTracer::new(self.context.clone())
    }
}

#[derive(Debug)]
pub struct UdpTracer {
    pool: ArcObjPool<BytesMut>,
    context: UdpContext,
}
impl UdpTracer {
    pub fn new(context: UdpContext) -> Self {
        let pool = ArcObjPool::new(
            None,
            NonZeroUsize::new(1).unwrap(),
            || BytesMut::with_capacity(PACKET_BUFFER_LENGTH),
            |buf| buf.clear(),
        );
        Self { pool, context }
    }
}
impl TraceRtt for UdpTracer {
    type Addr = InternetAddr;

    async fn trace_rtt(&self, chain: &UdpConnChain) -> Result<Duration, AnyError> {
        let mut pkt_buf = self.pool.take();
        let res = trace_rtt(&mut pkt_buf, chain, &self.context)
            .await
            .map_err(|e| e.into());
        self.pool.put(pkt_buf);
        res
    }
}
pub async fn trace_rtt(
    pkt_buf: &mut BytesMut,
    proxies: &UdpConnChain,
    context: &UdpContext,
) -> Result<Duration, TraceError> {
    if proxies.is_empty() {
        return Ok(Duration::from_secs(0));
    }

    // Connect to upstream
    let proxy_addr = &proxies[0].address;
    let addr = proxy_addr.to_socket_addr().await?;
    let upstream = context.connector.connect(addr).await?;

    // Convert addresses to headers
    let pairs = convert_proxies_to_header_crypto_pairs(proxies, None);

    // Save headers to buffer
    let mut buf = Vec::new();
    let mut writer = io::Cursor::new(&mut buf);
    for (header, crypto) in &pairs {
        let mut crypto_cursor = tokio_chacha20::cursor::EncryptCursor::new_x(*crypto.key());
        write_header(&mut writer, header, &mut crypto_cursor).unwrap();
    }

    let start = Instant::now();

    // Send request
    upstream.send(&buf).await?;

    // Recv response
    let n = upstream.recv_buf(pkt_buf).await?;

    let end = Instant::now();

    // Decode and check headers
    let mut reader = io::Cursor::new(&pkt_buf[..n]);
    for node in proxies.iter() {
        trace!(?node.address, "Reading response");
        let mut crypto_cursor =
            tokio_chacha20::cursor::DecryptCursor::new_x(*node.header_crypto.key());
        let validator = ValidatorRef::Time(&context.time_validator);
        let resp: RouteResponse = read_header(&mut reader, &mut crypto_cursor, &validator)?;
        if let Err(err) = resp.result {
            warn!(?err, %node.address, "Upstream responded with an error");
            return Err(TraceError::Response {
                err,
                addr: node.address.clone(),
            });
        }
    }

    counter!("udp.traces").increment(1);
    Ok(end.duration_since(start))
}
#[derive(Debug, Error)]
pub enum TraceError {
    #[error("IO error: {0}")]
    Io(#[from] io::Error),
    #[error("Failed to read response from upstream: {0}")]
    Header(#[from] CodecError),
    #[error("Upstream responded with an error: {err}, {addr}")]
    Response { err: RouteError, addr: InternetAddr },
}
