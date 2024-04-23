use std::{io, net::SocketAddr, ops::DerefMut, pin::Pin};

use tokio::io::{AsyncRead, AsyncWrite};

use common::stream::{IoAddr, IoStream};

use super::{
    addr::ConcreteStreamAddr,
    streams::{
        kcp::AddressedKcpStream, mptcp::IoMptcpStream, rtp::AddressedRtpStream, tcp::IoTcpStream,
    },
};

#[derive(Debug)]
pub enum Connection {
    // Quic(QuicIoStream),
    Tcp(IoTcpStream),
    Kcp(AddressedKcpStream),
    Mptcp(IoMptcpStream),
    Rtp(AddressedRtpStream),
}

impl IoStream for Connection {}
impl IoAddr for Connection {
    fn peer_addr(&self) -> io::Result<SocketAddr> {
        match self {
            // CreatedStream::Quic(x) => x.peer_addr(),
            Connection::Tcp(x) => x.peer_addr(),
            Connection::Kcp(x) => x.peer_addr(),
            Connection::Mptcp(x) => IoAddr::peer_addr(x),
            Connection::Rtp(x) => IoAddr::peer_addr(x),
        }
    }

    fn local_addr(&self) -> io::Result<SocketAddr> {
        match self {
            // CreatedStream::Quic(x) => x.local_addr(),
            Connection::Tcp(x) => x.local_addr(),
            Connection::Kcp(x) => x.local_addr(),
            Connection::Mptcp(x) => IoAddr::local_addr(x),
            Connection::Rtp(x) => IoAddr::local_addr(x),
        }
    }
}

impl AsyncWrite for Connection {
    fn poll_write(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> std::task::Poll<Result<usize, io::Error>> {
        match self.deref_mut() {
            // CreatedStream::Quic(x) => Pin::new(x).poll_write(cx, buf),
            Connection::Tcp(x) => Pin::new(x).poll_write(cx, buf),
            Connection::Kcp(x) => Pin::new(x).poll_write(cx, buf),
            Connection::Mptcp(x) => Pin::new(x).poll_write(cx, buf),
            Connection::Rtp(x) => Pin::new(x).poll_write(cx, buf),
        }
    }

    fn poll_flush(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), io::Error>> {
        match self.deref_mut() {
            // CreatedStream::Quic(x) => Pin::new(x).poll_flush(cx),
            Connection::Tcp(x) => Pin::new(x).poll_flush(cx),
            Connection::Kcp(x) => Pin::new(x).poll_flush(cx),
            Connection::Mptcp(x) => Pin::new(x).poll_flush(cx),
            Connection::Rtp(x) => Pin::new(x).poll_flush(cx),
        }
    }

    fn poll_shutdown(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), io::Error>> {
        match self.deref_mut() {
            // CreatedStream::Quic(x) => Pin::new(x).poll_shutdown(cx),
            Connection::Tcp(x) => Pin::new(x).poll_shutdown(cx),
            Connection::Kcp(x) => Pin::new(x).poll_shutdown(cx),
            Connection::Mptcp(x) => Pin::new(x).poll_shutdown(cx),
            Connection::Rtp(x) => Pin::new(x).poll_shutdown(cx),
        }
    }
}

impl AsyncRead for Connection {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<io::Result<()>> {
        match self.deref_mut() {
            // CreatedStream::Quic(x) => Pin::new(x).poll_read(cx, buf),
            Connection::Tcp(x) => Pin::new(x).poll_read(cx, buf),
            Connection::Kcp(x) => Pin::new(x).poll_read(cx, buf),
            Connection::Mptcp(x) => Pin::new(x).poll_read(cx, buf),
            Connection::Rtp(x) => Pin::new(x).poll_read(cx, buf),
        }
    }
}

#[derive(Debug)]
pub struct ConnAndAddr {
    pub stream: Connection,
    pub addr: ConcreteStreamAddr,
    pub sock_addr: SocketAddr,
}
