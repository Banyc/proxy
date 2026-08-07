use async_trait::async_trait;
use common::{
    addr::any_addr,
    connect::{ConnectorConfig, ConnectorResetSignal},
    proto::connect::stream::StreamConnect,
    stream::{ConnParts, HasIoAddr, OwnIoStream},
};
use std::{
    io,
    net::SocketAddr,
    pin::Pin,
    sync::Arc,
    task::{Context, Poll},
};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
#[derive(Debug)]
pub struct RtpMuxConnector {
    inner: Arc<::rtp_mux::RtpMuxConnector>,
}

/// The driver for a proxy [`RtpMuxConnector`].
///
/// It concurrently runs the inner [`rtp_mux::RtpMuxConnector`]'s connector
/// loop and the proxy reset listener that tears down sessions on a
/// [`ConnectorResetSignal`]. Spawn it into the parent runtime's
/// actively-reaped `JoinSet` so its exit is observed and its drop aborts
/// both child tasks.
#[must_use = "the connector is inert until the driver is spawned"]
pub struct RtpMuxConnectorDriver(Pin<Box<dyn std::future::Future<Output = ()> + Send + 'static>>);

impl std::future::Future for RtpMuxConnectorDriver {
    type Output = ();
    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        self.0.as_mut().poll(cx)
    }
}

impl RtpMuxConnector {
    pub fn new(
        config: Arc<std::sync::RwLock<ConnectorConfig>>,
        reset: ConnectorResetSignal,
        fec: bool,
    ) -> (Self, RtpMuxConnectorDriver) {
        let bind = Arc::new(move |addr: SocketAddr| {
            config
                .read()
                .unwrap()
                .bind
                .get_matched(&addr.ip())
                .map(|ip| SocketAddr::new(ip, 0))
                .unwrap_or_else(|| any_addr(&addr.ip()))
        });
        let (inner, inner_driver) = ::rtp_mux::RtpMuxConnector::new(bind, fec);
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
        let driver = RtpMuxConnectorDriver(Box::pin(async move {
            tokio::select! {
                () = inner_driver => {},
                () = reset_driver => {},
            }
        }));
        (Self { inner }, driver)
    }
}
#[async_trait]
impl StreamConnect for RtpMuxConnector {
    async fn connect(&self, addr: SocketAddr) -> io::Result<Box<dyn ConnParts>> {
        self.inner
            .connect_stream(addr)
            .await
            .map(ProxyRtpMuxClientStream)
            .map(|stream| Box::new(stream) as Box<dyn ConnParts>)
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
impl OwnIoStream for ProxyRtpMuxClientStream {}
impl ConnParts for ProxyRtpMuxClientStream {
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
