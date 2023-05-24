use std::{fmt::Display, io, net::SocketAddr, ops::DerefMut, pin::Pin};

use async_trait::async_trait;
use bytesize::ByteSize;
use tokio::{
    io::{AsyncRead, AsyncWrite},
    net::TcpStream,
};
use tracing::error;

use crate::{error::ProxyProtocolError, header::InternetAddr};

use self::{pool::Pool, quic::QuicIoStream, tcp::TcpConnector};

pub mod pool;
pub mod quic;
pub mod tcp;
pub mod xor;

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
}

#[async_trait]
impl ConnectStream for StreamConnector {
    async fn connect(&self, addr: SocketAddr) -> io::Result<CreatedStream> {
        match self {
            StreamConnector::Tcp(x) => x.connect(addr).await,
        }
    }
}

pub async fn connect_with_pool<C>(
    connector: &C,
    addr: &InternetAddr,
    stream_pool: &Pool,
    allow_loopback: bool,
) -> Result<(CreatedStream, SocketAddr), ProxyProtocolError>
where
    C: ConnectStream,
{
    let stream = stream_pool.open_stream(addr).await;
    let ret = match stream {
        Some((stream, sock_addr)) => (stream, sock_addr),
        None => {
            let sock_addr = addr
                .to_socket_addr()
                .await
                .inspect_err(|e| error!(?e, ?addr, "Failed to resolve address"))?;
            if !allow_loopback && sock_addr.ip().is_loopback() {
                // Prevent connections to localhost
                error!(?addr, "Refusing to connect to loopback address");
                return Err(ProxyProtocolError::Loopback);
            }
            let stream = connector.connect(sock_addr).await?;
            (stream, sock_addr)
        }
    };
    Ok(ret)
}

#[async_trait]
pub trait StreamServerHook {
    async fn handle_stream<S>(&self, stream: S)
    where
        S: IoStream + IoAddr;
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct StreamMetrics {
    pub start: std::time::Instant,
    pub end: std::time::Instant,
    pub bytes_uplink: u64,
    pub bytes_downlink: u64,
    pub upstream_addr: InternetAddr,
    pub upstream_sock_addr: SocketAddr,
    pub downstream_addr: SocketAddr,
}

impl Display for StreamMetrics {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let duration = self.end - self.start;
        let duration = duration.as_secs_f64();
        let uplink_speed = self.bytes_uplink as f64 / duration;
        let downlink_speed = self.bytes_downlink as f64 / duration;
        let upstream_addr = self.upstream_addr.to_string();
        let upstream_sock_addr = self.upstream_sock_addr.to_string();
        let upstream_addrs = match upstream_addr == upstream_sock_addr {
            true => upstream_addr,
            false => format!("{}, {}", upstream_addr, upstream_sock_addr),
        };
        write!(
            f,
            "up: {{ {}, {}/s }}, down: {{ {}, {}/s }}, duration: {:.1} s, upstream: {{ {} }}, downstream: {}",
            ByteSize::b(self.bytes_uplink),
            ByteSize::b(uplink_speed as u64),
            ByteSize::b(self.bytes_downlink),
            ByteSize::b(downlink_speed as u64),
            duration,
            upstream_addrs,
            self.downstream_addr
        )
    }
}

#[derive(Debug)]
pub enum CreatedStream {
    Quic(QuicIoStream),
    Tcp(TcpStream),
}

impl IoStream for CreatedStream {}
impl IoAddr for CreatedStream {
    fn peer_addr(&self) -> io::Result<SocketAddr> {
        match self {
            CreatedStream::Quic(x) => x.peer_addr(),
            CreatedStream::Tcp(x) => x.peer_addr(),
        }
    }

    fn local_addr(&self) -> io::Result<SocketAddr> {
        match self {
            CreatedStream::Quic(x) => x.local_addr(),
            CreatedStream::Tcp(x) => x.local_addr(),
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
        }
    }

    fn poll_flush(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), io::Error>> {
        match self.deref_mut() {
            CreatedStream::Quic(x) => Pin::new(x).poll_flush(cx),
            CreatedStream::Tcp(x) => Pin::new(x).poll_flush(cx),
        }
    }

    fn poll_shutdown(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), io::Error>> {
        match self.deref_mut() {
            CreatedStream::Quic(x) => Pin::new(x).poll_shutdown(cx),
            CreatedStream::Tcp(x) => Pin::new(x).poll_shutdown(cx),
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
        }
    }
}
