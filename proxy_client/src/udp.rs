use std::{
    io::{self, Write},
    net::SocketAddr,
    num::NonZeroUsize,
    sync::Arc,
    time::{Duration, Instant},
};

use bytes::BytesMut;
use common::{
    addr::{any_addr, InternetAddr},
    anti_replay::{ReplayValidator, VALIDATOR_CAPACITY, VALIDATOR_TIME_FRAME},
    error::AnyError,
    header::{
        codec::{read_header, write_header, CodecError},
        route::{RouteError, RouteResponse},
    },
    proxy_table::{convert_proxies_to_header_crypto_pairs, Tracer, TracerBuilder},
    udp::{
        io_copy::{UdpRecv, UdpSend},
        proxy_table::UdpProxyChain,
        PACKET_BUFFER_LENGTH,
    },
};
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
        proxies: Arc<UdpProxyChain>,
        destination: InternetAddr,
    ) -> Result<UdpProxyClient, EstablishError> {
        // If there are no proxy configs, just connect to the destination
        if proxies.is_empty() {
            let addr = destination.to_socket_addr().await.map_err(|e| {
                EstablishError::ResolveDestination {
                    source: e,
                    addr: destination.clone(),
                }
            })?;
            let any_addr = any_addr(&addr.ip());
            let upstream = UdpSocket::bind(any_addr)
                .await
                .map_err(EstablishError::ClientBindAny)?;
            upstream
                .connect(addr)
                .await
                .map_err(|e| EstablishError::ConnectDestination {
                    source: e,
                    addr: destination.clone(),
                    sock_addr: addr,
                })?;

            let upstream = Arc::new(upstream);
            let write = UdpProxyClientWriteHalf::new(upstream.clone(), Vec::new().into());
            let read = UdpProxyClientReadHalf::new(upstream, Vec::new().into(), proxies);
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
        let any_addr = any_addr(&addr.ip());
        let upstream = UdpSocket::bind(any_addr)
            .await
            .map_err(EstablishError::ClientBindAny)?;
        upstream
            .connect(addr)
            .await
            .map_err(|e| EstablishError::ConnectFirstProxy {
                source: e,
                addr: proxy_addr.clone(),
                sock_addr: addr,
            })?;

        // Convert addresses to headers
        let pairs = convert_proxies_to_header_crypto_pairs(&proxies, Some(destination));

        // Save headers to buffer
        let mut buf = Vec::new();
        let mut writer = io::Cursor::new(&mut buf);
        for (header, crypto) in &pairs {
            trace!(?header, "Writing header to buffer");
            let mut crypto_cursor = tokio_chacha20::cursor::EncryptCursor::new_x(*crypto.key());
            write_header(&mut writer, header, &mut crypto_cursor).unwrap();
        }

        // Return stream
        let upstream = Arc::new(upstream);
        let header_bytes: Arc<[_]> = buf.into();
        let write = UdpProxyClientWriteHalf::new(upstream.clone(), header_bytes.clone());
        let read = UdpProxyClientReadHalf::new(upstream, header_bytes, proxies);
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
    #[error("Failed to created a client socket: {0}")]
    ClientBindAny(#[source] io::Error),
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

#[derive(Debug)]
pub struct UdpProxyClientWriteHalf {
    upstream: Arc<UdpSocket>,
    headers_bytes: Arc<[u8]>,
    write_buf: Vec<u8>,
}
impl UdpSend for UdpProxyClientWriteHalf {
    async fn trait_send(&mut self, buf: &[u8]) -> Result<usize, AnyError> {
        Self::send(self, buf).await.map_err(|e| e.into())
    }
}
impl UdpProxyClientWriteHalf {
    pub fn new(upstream: Arc<UdpSocket>, headers_bytes: Arc<[u8]>) -> Self {
        Self {
            upstream,
            headers_bytes,
            write_buf: vec![],
        }
    }

    #[instrument(skip_all)]
    pub async fn send(&mut self, buf: &[u8]) -> Result<usize, SendError> {
        self.write_buf.clear();

        // Write header
        self.write_buf.write_all(&self.headers_bytes).unwrap();

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

#[derive(Debug)]
pub struct UdpProxyClientReadHalf {
    upstream: Arc<UdpSocket>,
    headers_bytes: Arc<[u8]>,
    proxies: Arc<UdpProxyChain>,
    read_buf: Vec<u8>,
    _replay_validator: ReplayValidator,
}
impl UdpRecv for UdpProxyClientReadHalf {
    async fn trait_recv(&mut self, buf: &mut [u8]) -> Result<usize, AnyError> {
        Self::recv(self, buf).await.map_err(|e| e.into())
    }
}
impl UdpProxyClientReadHalf {
    pub fn new(
        upstream: Arc<UdpSocket>,
        headers_bytes: Arc<[u8]>,
        proxies: Arc<UdpProxyChain>,
    ) -> Self {
        Self {
            upstream,
            headers_bytes,
            proxies,
            read_buf: vec![],
            _replay_validator: ReplayValidator::new(VALIDATOR_TIME_FRAME, VALIDATOR_CAPACITY),
        }
    }

    #[instrument(skip_all)]
    pub async fn recv(&mut self, buf: &mut [u8]) -> Result<usize, RecvError> {
        let cap = self.headers_bytes.len() + buf.len();
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
            let resp: RouteResponse = read_header(&mut reader, &mut crypto_cursor, None)?;
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

pub struct UdpTracerBuilder {}
impl UdpTracerBuilder {
    pub fn new() -> Self {
        Self {}
    }
}
impl Default for UdpTracerBuilder {
    fn default() -> Self {
        Self::new()
    }
}
impl TracerBuilder for UdpTracerBuilder {
    type Tracer = UdpTracer;

    fn build(&self) -> Self::Tracer {
        UdpTracer::new()
    }
}

#[derive(Debug)]
pub struct UdpTracer {
    pool: ArcObjPool<BytesMut>,
    replay_validator: ReplayValidator,
}
impl UdpTracer {
    pub fn new() -> Self {
        let pool = ArcObjPool::new(
            None,
            NonZeroUsize::new(1).unwrap(),
            || BytesMut::with_capacity(PACKET_BUFFER_LENGTH),
            |buf| buf.clear(),
        );
        Self {
            pool,
            replay_validator: ReplayValidator::new(VALIDATOR_TIME_FRAME, VALIDATOR_CAPACITY),
        }
    }
}
impl Default for UdpTracer {
    fn default() -> Self {
        Self::new()
    }
}
impl Tracer for UdpTracer {
    type Address = InternetAddr;

    async fn trace_rtt(&self, chain: &UdpProxyChain) -> Result<Duration, AnyError> {
        let mut pkt_buf = self.pool.take();
        let res = trace_rtt(&mut pkt_buf, chain, &self.replay_validator)
            .await
            .map_err(|e| e.into());
        self.pool.put(pkt_buf);
        res
    }
}
pub async fn trace_rtt(
    pkt_buf: &mut BytesMut,
    proxies: &UdpProxyChain,
    _replay_validator: &ReplayValidator,
) -> Result<Duration, TraceError> {
    if proxies.is_empty() {
        return Ok(Duration::from_secs(0));
    }

    // Connect to upstream
    let proxy_addr = &proxies[0].address;
    let addr = proxy_addr.to_socket_addr().await?;
    let any_addr = any_addr(&addr.ip());
    let upstream = UdpSocket::bind(any_addr).await?;
    upstream.connect(addr).await?;

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
        let resp: RouteResponse = read_header(&mut reader, &mut crypto_cursor, None)?;
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
