use std::{io, net::SocketAddr, ops::DerefMut, pin::Pin};

use tokio::io::{AsyncRead, AsyncWrite};

use common::stream::{HasIoAddr, OwnIoStream};

use super::{
    addr::ConcreteStreamAddr,
    streams::{
        kcp::AddressedKcpStream, mptcp::IoMptcpStream, mux::IoMuxStream, rtp::AddressedRtpStream,
        tcp::IoTcpStream,
    },
};

#[derive(Debug)]
pub enum Conn {
    // Quic(QuicIoStream),
    Tcp(IoTcpStream),
    Mux(IoMuxStream),
    Kcp(AddressedKcpStream),
    Mptcp(IoMptcpStream),
    Rtp(AddressedRtpStream),
}
impl OwnIoStream for Conn {}
impl HasIoAddr for Conn {
    fn peer_addr(&self) -> io::Result<SocketAddr> {
        match self {
            // CreatedStream::Quic(x) => x.peer_addr(),
            Conn::Tcp(x) => x.peer_addr(),
            Conn::Mux(x) => x.peer_addr(),
            Conn::Kcp(x) => x.peer_addr(),
            Conn::Mptcp(x) => HasIoAddr::peer_addr(x),
            Conn::Rtp(x) => HasIoAddr::peer_addr(x),
        }
    }

    fn local_addr(&self) -> io::Result<SocketAddr> {
        match self {
            // CreatedStream::Quic(x) => x.local_addr(),
            Conn::Tcp(x) => x.local_addr(),
            Conn::Mux(x) => x.local_addr(),
            Conn::Kcp(x) => x.local_addr(),
            Conn::Mptcp(x) => HasIoAddr::local_addr(x),
            Conn::Rtp(x) => HasIoAddr::local_addr(x),
        }
    }
}
impl AsyncWrite for Conn {
    fn poll_write(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> std::task::Poll<Result<usize, io::Error>> {
        match self.deref_mut() {
            // CreatedStream::Quic(x) => Pin::new(x).poll_write(cx, buf),
            Conn::Tcp(x) => Pin::new(x).poll_write(cx, buf),
            Conn::Mux(x) => Pin::new(x).poll_write(cx, buf),
            Conn::Kcp(x) => Pin::new(x).poll_write(cx, buf),
            Conn::Mptcp(x) => Pin::new(x).poll_write(cx, buf),
            Conn::Rtp(x) => Pin::new(x).poll_write(cx, buf),
        }
    }

    fn poll_flush(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), io::Error>> {
        match self.deref_mut() {
            // CreatedStream::Quic(x) => Pin::new(x).poll_flush(cx),
            Conn::Tcp(x) => Pin::new(x).poll_flush(cx),
            Conn::Mux(x) => Pin::new(x).poll_flush(cx),
            Conn::Kcp(x) => Pin::new(x).poll_flush(cx),
            Conn::Mptcp(x) => Pin::new(x).poll_flush(cx),
            Conn::Rtp(x) => Pin::new(x).poll_flush(cx),
        }
    }

    fn poll_shutdown(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), io::Error>> {
        match self.deref_mut() {
            // CreatedStream::Quic(x) => Pin::new(x).poll_shutdown(cx),
            Conn::Tcp(x) => Pin::new(x).poll_shutdown(cx),
            Conn::Mux(x) => Pin::new(x).poll_shutdown(cx),
            Conn::Kcp(x) => Pin::new(x).poll_shutdown(cx),
            Conn::Mptcp(x) => Pin::new(x).poll_shutdown(cx),
            Conn::Rtp(x) => Pin::new(x).poll_shutdown(cx),
        }
    }
}
impl AsyncRead for Conn {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<io::Result<()>> {
        match self.deref_mut() {
            // CreatedStream::Quic(x) => Pin::new(x).poll_read(cx, buf),
            Conn::Tcp(x) => Pin::new(x).poll_read(cx, buf),
            Conn::Mux(x) => Pin::new(x).poll_read(cx, buf),
            Conn::Kcp(x) => Pin::new(x).poll_read(cx, buf),
            Conn::Mptcp(x) => Pin::new(x).poll_read(cx, buf),
            Conn::Rtp(x) => Pin::new(x).poll_read(cx, buf),
        }
    }
}

#[derive(Debug)]
pub struct ConnAndAddr {
    pub stream: Conn,
    pub addr: ConcreteStreamAddr,
    pub sock_addr: SocketAddr,
}
