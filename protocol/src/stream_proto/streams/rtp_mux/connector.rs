use async_trait::async_trait;
use common::{
    addr::any_addr,
    connect::{ConnectorConfigReader, ConnectorResetSignal},
    proxy_runtime::connect::{
        stream::StreamConnect,
        udp::{UdpConnection, UdpMuxDialer},
    },
    stream_runtime::{HasIoAddr, IoConnection, OwnedIoStream},
};
use rtp_mux::LaneClass;
use std::{
    io,
    net::SocketAddr,
    pin::Pin,
    sync::Arc,
    task::{Context, Poll},
};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

use crate::stream_proto::streams::mux::{
    ConnectorDriverError, MuxConnectorDriver, MuxFlowKind, write_flow_kind,
};
#[derive(Debug)]
pub struct RtpMuxConnector {
    inner: Arc<::rtp_mux::RtpMuxConnector>,
}

impl RtpMuxConnector {
    pub fn new(
        config: ConnectorConfigReader,
        reset: ConnectorResetSignal,
    ) -> (Self, MuxConnectorDriver) {
        let bind = Arc::new(move |addr: SocketAddr| {
            config
                .current()
                .bind
                .get_matched(&addr.ip())
                .map(|ip| SocketAddr::new(ip, 0))
                .unwrap_or_else(|| any_addr(&addr.ip()))
        });
        let (inner, inner_driver) = ::rtp_mux::RtpMuxConnector::with_config(
            ::rtp_mux::RtpMuxConnectorConfig::standard(bind),
        );
        let inner = Arc::new(inner);
        let reset_driver = {
            let inner = Arc::clone(&inner);
            async move {
                let mut waiter = reset.0.subscription();
                loop {
                    waiter.notified().await;
                    if inner.reset().await.is_err() {
                        break;
                    }
                }
            }
        };
        let driver = MuxConnectorDriver::new(async move {
            tokio::select! {
                () = inner_driver => ConnectorDriverError::ConnectorExited,
                () = reset_driver => ConnectorDriverError::ResetListenerExited,
            }
        });
        (Self { inner }, driver)
    }
}
#[async_trait]
impl StreamConnect for RtpMuxConnector {
    async fn connect(
        &self,
        addr: SocketAddr,
        obfuscation_key: Option<[u8; 32]>,
    ) -> io::Result<Box<dyn IoConnection>> {
        let mut stream = self
            .inner
            .connect_stream_with_lane_and_key(
                addr,
                LaneClass::Interactive,
                obfuscation_key.map(::rtp_mux::ObfuscationKey::from_bytes),
            )
            .await?;
        write_flow_kind(&mut stream, MuxFlowKind::Stream).await?;
        Ok(Box::new(ProxyRtpMuxClientStream(stream)))
    }
    fn reset_addr(&self, addr: SocketAddr) {
        self.inner.force_redial(addr);
    }
    fn reoptimize(&self, addr: SocketAddr) {
        self.inner.reoptimize(addr);
    }
    fn session_stats(&self, addr: SocketAddr) -> Option<String> {
        self.inner
            .probe_session(addr)
            .and_then(|session| session.stats())
            .map(|stats| stats.to_string())
    }
    fn reports_session_stats(&self) -> bool {
        true
    }
}
#[async_trait]
impl UdpMuxDialer for RtpMuxConnector {
    async fn dial_udp(
        &self,
        addr: SocketAddr,
        obfuscation_key: Option<[u8; 32]>,
    ) -> io::Result<UdpConnection> {
        let mut stream = self
            .inner
            .connect_stream_with_lane_and_key(
                addr,
                LaneClass::Interactive,
                obfuscation_key.map(::rtp_mux::ObfuscationKey::from_bytes),
            )
            .await?;
        write_flow_kind(&mut stream, MuxFlowKind::Udp).await?;
        let addr = stream.addr();
        let (reader, writer) = tokio::io::split(stream);
        Ok(UdpConnection::mux_io(
            reader,
            writer,
            addr.local_addr,
            addr.peer_addr,
        ))
    }
}
#[derive(Debug)]
struct ProxyRtpMuxClientStream(::rtp_mux::ClientStream);
impl AsyncRead for ProxyRtpMuxClientStream {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        Pin::new(&mut self.0).poll_read(cx, buf)
    }
}
impl AsyncWrite for ProxyRtpMuxClientStream {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.0).poll_write(cx, buf)
    }
    fn poll_write_vectored(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[io::IoSlice<'_>],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.0).poll_write_vectored(cx, bufs)
    }
    fn is_write_vectored(&self) -> bool {
        self.0.is_write_vectored()
    }
    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.0).poll_flush(cx)
    }
    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.0).poll_shutdown(cx)
    }
}
impl OwnedIoStream for ProxyRtpMuxClientStream {}
impl IoConnection for ProxyRtpMuxClientStream {
    fn set_stream_name(&self, name: &str) {
        self.0.set_name(name);
    }
}
impl HasIoAddr for ProxyRtpMuxClientStream {
    fn peer_addr(&self) -> io::Result<SocketAddr> {
        Ok(self.0.addr().peer_addr)
    }
    fn local_addr(&self) -> io::Result<SocketAddr> {
        Ok(self.0.addr().local_addr)
    }
}
