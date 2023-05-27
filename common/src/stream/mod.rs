use std::{fmt::Display, io, net::SocketAddr, ops::DerefMut, pin::Pin};

use async_trait::async_trait;
use bytesize::ByteSize;
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::{
    io::{AsyncRead, AsyncWrite},
    net::TcpStream,
};
use tracing::error;

use crate::addr::InternetAddr;

use self::{
    header::StreamType,
    kcp::{AddressedKcpStream, KcpConnector},
    pool::Pool,
    quic::QuicIoStream,
    tcp::TcpConnector,
};

pub mod header;
pub mod kcp;
pub mod pool;
pub mod quic;
pub mod tcp;
pub mod xor;

#[derive(Debug, Clone, PartialEq, Eq, Hash, Deserialize, Serialize)]
pub struct StreamAddrBuilder {
    pub address: String,
    pub stream_type: StreamType,
}

impl StreamAddrBuilder {
    pub fn build(self) -> StreamAddr {
        StreamAddr {
            address: self.address.into(),
            stream_type: self.stream_type,
        }
    }
}

/// A stream address
#[derive(Debug, Clone, PartialEq, Eq, Hash, Deserialize, Serialize)]
pub struct StreamAddr {
    pub address: InternetAddr,
    pub stream_type: StreamType,
}

impl Display for StreamAddr {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.stream_type {
            StreamType::Tcp => write!(f, "tcp://{}", self.address),
            StreamType::Kcp => write!(f, "kcp://{}", self.address),
        }
    }
}

pub trait IoStream: AsyncRead + AsyncWrite + Unpin + Send + Sync + 'static {}

pub trait IoAddr {
    fn peer_addr(&self) -> io::Result<SocketAddr>;
    fn local_addr(&self) -> io::Result<SocketAddr>;
}

#[async_trait]
pub trait ConnectStream {
    async fn connect(&self, addr: SocketAddr) -> io::Result<CreatedStream>;
}

#[derive(Debug)]
pub enum StreamConnector {
    Tcp(TcpConnector),
    Kcp(KcpConnector),
}

impl StreamConnector {
    pub fn new() -> Self {
        Self::Tcp(TcpConnector)
    }
}

impl Default for StreamConnector {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl ConnectStream for StreamConnector {
    async fn connect(&self, addr: SocketAddr) -> io::Result<CreatedStream> {
        match self {
            StreamConnector::Tcp(x) => x.connect(addr).await,
            StreamConnector::Kcp(x) => x.connect(addr).await,
        }
    }
}

impl From<StreamType> for StreamConnector {
    fn from(value: StreamType) -> Self {
        match value {
            StreamType::Tcp => StreamConnector::Tcp(TcpConnector),
            StreamType::Kcp => StreamConnector::Kcp(KcpConnector),
        }
    }
}

pub async fn connect_with_pool(
    addr: &StreamAddr,
    stream_pool: &Pool,
    allow_loopback: bool,
) -> Result<(CreatedStream, SocketAddr), ConnectError> {
    let stream = stream_pool.open_stream(addr).await;
    let ret = match stream {
        Some((stream, sock_addr)) => (stream, sock_addr),
        None => {
            let connector: StreamConnector = addr.stream_type.into();
            let sock_addr =
                addr.address
                    .to_socket_addr()
                    .await
                    .map_err(|e| ConnectError::ResolveAddr {
                        source: e,
                        addr: addr.clone(),
                    })?;
            if !allow_loopback && sock_addr.ip().is_loopback() {
                // Prevent connections to localhost
                return Err(ConnectError::Loopback {
                    addr: addr.clone(),
                    sock_addr,
                });
            }
            let stream =
                connector
                    .connect(sock_addr)
                    .await
                    .map_err(|e| ConnectError::ConnectAddr {
                        source: e,
                        addr: addr.clone(),
                        sock_addr,
                    })?;
            (stream, sock_addr)
        }
    };
    Ok(ret)
}

#[derive(Debug, Error)]
pub enum ConnectError {
    #[error("Failed to resolve address")]
    ResolveAddr {
        #[source]
        source: io::Error,
        addr: StreamAddr,
    },
    #[error("Refused to connect to loopback address")]
    Loopback {
        addr: StreamAddr,
        sock_addr: SocketAddr,
    },
    #[error("Failed to connect to address")]
    ConnectAddr {
        #[source]
        source: io::Error,
        addr: StreamAddr,
        sock_addr: SocketAddr,
    },
}

#[async_trait]
pub trait StreamServerHook {
    async fn handle_stream<S>(&self, stream: S)
    where
        S: IoStream + IoAddr + std::fmt::Debug;
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct StreamMetrics {
    pub start: std::time::Instant,
    pub end: std::time::Instant,
    pub bytes_uplink: u64,
    pub bytes_downlink: u64,
    pub upstream_addr: StreamAddr,
    pub upstream_sock_addr: SocketAddr,
    pub downstream_addr: Option<SocketAddr>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct FailedStreamMetrics {
    pub start: std::time::Instant,
    pub end: std::time::Instant,
    pub upstream_addr: StreamAddr,
    pub upstream_sock_addr: SocketAddr,
    pub downstream_addr: Option<SocketAddr>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct TunnelMetrics {
    pub stream: StreamMetrics,
    pub destination: InternetAddr,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct FailedTunnelMetrics {
    pub stream: FailedStreamMetrics,
    pub destination: InternetAddr,
}

impl Display for StreamMetrics {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let duration = self.end - self.start;
        let duration = duration.as_secs_f64();
        let uplink_speed = self.bytes_uplink as f64 / duration;
        let downlink_speed = self.bytes_downlink as f64 / duration;
        let upstream_addrs = match &self.upstream_addr.address {
            InternetAddr::SocketAddr(v) => v.to_string(),
            InternetAddr::String(_) => {
                format!("{},{}", self.upstream_addr, self.upstream_sock_addr.ip())
            }
        };
        write!(
            f,
            "{:.1}s,up{{{},{}/s}},dn{{{},{}/s}},up{{{}}}",
            duration,
            ByteSize::b(self.bytes_uplink),
            ByteSize::b(uplink_speed as u64),
            ByteSize::b(self.bytes_downlink),
            ByteSize::b(downlink_speed as u64),
            upstream_addrs,
        )?;
        if let Some(downstream_addr) = self.downstream_addr {
            write!(f, ",dn:{}", downstream_addr)?;
        }
        Ok(())
    }
}

impl Display for FailedStreamMetrics {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let duration = self.end - self.start;
        let duration = duration.as_secs_f64();
        let upstream_addrs = match &self.upstream_addr.address {
            InternetAddr::SocketAddr(v) => v.to_string(),
            InternetAddr::String(_) => {
                format!("{},{}", self.upstream_addr, self.upstream_sock_addr.ip())
            }
        };
        write!(f, "{:.1}s,up{{{}}}", duration, upstream_addrs)?;
        if let Some(downstream_addr) = self.downstream_addr {
            write!(f, ",dn:{}", downstream_addr)?;
        }
        Ok(())
    }
}

impl Display for TunnelMetrics {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.stream.to_string())?;
        write!(f, ",dt:{}", self.destination)?;
        Ok(())
    }
}

impl Display for FailedTunnelMetrics {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.stream.to_string())?;
        write!(f, ",dt:{}", self.destination)?;
        Ok(())
    }
}

#[derive(Debug)]
pub enum CreatedStream {
    Quic(QuicIoStream),
    Tcp(TcpStream),
    Kcp(AddressedKcpStream),
}

impl IoStream for CreatedStream {}
impl IoAddr for CreatedStream {
    fn peer_addr(&self) -> io::Result<SocketAddr> {
        match self {
            CreatedStream::Quic(x) => x.peer_addr(),
            CreatedStream::Tcp(x) => x.peer_addr(),
            CreatedStream::Kcp(x) => x.peer_addr(),
        }
    }

    fn local_addr(&self) -> io::Result<SocketAddr> {
        match self {
            CreatedStream::Quic(x) => x.local_addr(),
            CreatedStream::Tcp(x) => x.local_addr(),
            CreatedStream::Kcp(x) => x.local_addr(),
        }
    }
}

impl AsyncWrite for CreatedStream {
    fn poll_write(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> std::task::Poll<Result<usize, io::Error>> {
        match self.deref_mut() {
            CreatedStream::Quic(x) => Pin::new(x).poll_write(cx, buf),
            CreatedStream::Tcp(x) => Pin::new(x).poll_write(cx, buf),
            CreatedStream::Kcp(x) => Pin::new(x).poll_write(cx, buf),
        }
    }

    fn poll_flush(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), io::Error>> {
        match self.deref_mut() {
            CreatedStream::Quic(x) => Pin::new(x).poll_flush(cx),
            CreatedStream::Tcp(x) => Pin::new(x).poll_flush(cx),
            CreatedStream::Kcp(x) => Pin::new(x).poll_flush(cx),
        }
    }

    fn poll_shutdown(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), io::Error>> {
        match self.deref_mut() {
            CreatedStream::Quic(x) => Pin::new(x).poll_shutdown(cx),
            CreatedStream::Tcp(x) => Pin::new(x).poll_shutdown(cx),
            CreatedStream::Kcp(x) => Pin::new(x).poll_shutdown(cx),
        }
    }
}

impl AsyncRead for CreatedStream {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<io::Result<()>> {
        match self.deref_mut() {
            CreatedStream::Quic(x) => Pin::new(x).poll_read(cx, buf),
            CreatedStream::Tcp(x) => Pin::new(x).poll_read(cx, buf),
            CreatedStream::Kcp(x) => Pin::new(x).poll_read(cx, buf),
        }
    }
}
