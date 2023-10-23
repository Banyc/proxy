use std::{
    fmt::Display,
    io,
    net::SocketAddr,
    ops::{Deref, DerefMut},
    pin::Pin,
};

use async_trait::async_trait;
use bytesize::ByteSize;
use mptcp::MptcpStream;
use tokio::{
    io::{AsyncRead, AsyncWrite},
    net::TcpStream,
};

use crate::{
    addr::{InternetAddr, InternetAddrKind},
    loading,
};

use self::{addr::StreamAddr, streams::kcp::AddressedKcpStream};

pub mod addr;
pub mod connect;
pub mod header;
pub mod io_copy;
pub mod pool;
pub mod proxy_table;
pub mod session_table;
pub mod steer;
pub mod streams;

pub trait IoStream: AsyncRead + AsyncWrite + Unpin + Send + Sync + 'static {}

pub trait IoAddr {
    fn peer_addr(&self) -> io::Result<SocketAddr>;
    fn local_addr(&self) -> io::Result<SocketAddr>;
}

#[async_trait]
pub trait StreamServerHook: loading::Hook {
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
pub struct SimplifiedStreamMetrics {
    pub start: std::time::Instant,
    pub end: std::time::Instant,
    pub upstream_addr: StreamAddr,
    pub upstream_sock_addr: SocketAddr,
    pub downstream_addr: Option<SocketAddr>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct StreamProxyMetrics {
    pub stream: StreamMetrics,
    pub destination: InternetAddr,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct SimplifiedStreamProxyMetrics {
    pub stream: SimplifiedStreamMetrics,
    pub destination: InternetAddr,
}

impl Display for StreamMetrics {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let duration = self.end - self.start;
        let duration = duration.as_secs_f64();
        let uplink_speed = self.bytes_uplink as f64 / duration;
        let downlink_speed = self.bytes_downlink as f64 / duration;
        let upstream_addrs = match self.upstream_addr.address.deref() {
            InternetAddrKind::SocketAddr(_) => self.upstream_addr.to_string(),
            InternetAddrKind::DomainName { .. } => {
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

impl Display for SimplifiedStreamMetrics {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let duration = self.end - self.start;
        let duration = duration.as_secs_f64();
        let upstream_addrs = match self.upstream_addr.address.deref() {
            InternetAddrKind::SocketAddr(_) => self.upstream_addr.to_string(),
            InternetAddrKind::DomainName { .. } => {
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

impl Display for StreamProxyMetrics {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.stream.to_string())?;
        write!(f, ",dt:{}", self.destination)?;
        Ok(())
    }
}

impl Display for SimplifiedStreamProxyMetrics {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.stream.to_string())?;
        write!(f, ",dt:{}", self.destination)?;
        Ok(())
    }
}

#[derive(Debug)]
pub enum CreatedStream {
    // Quic(QuicIoStream),
    Tcp(TcpStream),
    Kcp(AddressedKcpStream),
    Mptcp(MptcpStream),
}

impl IoStream for CreatedStream {}
impl IoAddr for CreatedStream {
    fn peer_addr(&self) -> io::Result<SocketAddr> {
        match self {
            // CreatedStream::Quic(x) => x.peer_addr(),
            CreatedStream::Tcp(x) => x.peer_addr(),
            CreatedStream::Kcp(x) => x.peer_addr(),
            CreatedStream::Mptcp(x) => IoAddr::peer_addr(x),
        }
    }

    fn local_addr(&self) -> io::Result<SocketAddr> {
        match self {
            // CreatedStream::Quic(x) => x.local_addr(),
            CreatedStream::Tcp(x) => x.local_addr(),
            CreatedStream::Kcp(x) => x.local_addr(),
            CreatedStream::Mptcp(x) => IoAddr::local_addr(x),
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
            // CreatedStream::Quic(x) => Pin::new(x).poll_write(cx, buf),
            CreatedStream::Tcp(x) => Pin::new(x).poll_write(cx, buf),
            CreatedStream::Kcp(x) => Pin::new(x).poll_write(cx, buf),
            CreatedStream::Mptcp(x) => Pin::new(x).poll_write(cx, buf),
        }
    }

    fn poll_flush(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), io::Error>> {
        match self.deref_mut() {
            // CreatedStream::Quic(x) => Pin::new(x).poll_flush(cx),
            CreatedStream::Tcp(x) => Pin::new(x).poll_flush(cx),
            CreatedStream::Kcp(x) => Pin::new(x).poll_flush(cx),
            CreatedStream::Mptcp(x) => Pin::new(x).poll_flush(cx),
        }
    }

    fn poll_shutdown(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), io::Error>> {
        match self.deref_mut() {
            // CreatedStream::Quic(x) => Pin::new(x).poll_shutdown(cx),
            CreatedStream::Tcp(x) => Pin::new(x).poll_shutdown(cx),
            CreatedStream::Kcp(x) => Pin::new(x).poll_shutdown(cx),
            CreatedStream::Mptcp(x) => Pin::new(x).poll_shutdown(cx),
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
            // CreatedStream::Quic(x) => Pin::new(x).poll_read(cx, buf),
            CreatedStream::Tcp(x) => Pin::new(x).poll_read(cx, buf),
            CreatedStream::Kcp(x) => Pin::new(x).poll_read(cx, buf),
            CreatedStream::Mptcp(x) => Pin::new(x).poll_read(cx, buf),
        }
    }
}

#[derive(Debug)]
pub struct CreatedStreamAndAddr {
    pub stream: CreatedStream,
    pub addr: StreamAddr,
    pub sock_addr: SocketAddr,
}
