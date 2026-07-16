use async_trait::async_trait;
use common::{
    addr::any_addr,
    connect::{ConnectorConfig, ConnectorReset},
    proto::connect::stream::StreamConnect,
    stream::{AsConn, HasIoAddr, OwnIoStream},
};
use std::{
    io,
    net::SocketAddr,
    pin::Pin,
    sync::Arc,
    task::{Context, Poll},
};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::task::JoinSet;
#[derive(Debug)]
pub struct RtpMuxConnector {
    inner: Arc<::rtp_mux::RtpMuxConnector>,
    _reset: JoinSet<()>,
}
impl RtpMuxConnector {
    pub fn new(
        config: Arc<std::sync::RwLock<ConnectorConfig>>,
        reset: ConnectorReset,
        fec: bool,
    ) -> Self {
        let bind = Arc::new(move |addr: SocketAddr| {
            config
                .read()
                .unwrap()
                .bind
                .get_matched(&addr.ip())
                .map(|ip| SocketAddr::new(ip, 0))
                .unwrap_or_else(|| any_addr(&addr.ip()))
        });
        let inner = Arc::new(::rtp_mux::RtpMuxConnector::new(bind, fec));
        let mut reset_tasks = JoinSet::new();
        {
            let inner = Arc::clone(&inner);
            reset_tasks.spawn(async move {
                let mut waiter = reset.0.waiter();
                loop {
                    waiter.notified().await;
                    if inner.reset().await.is_err() {
                        break;
                    }
                }
            });
        }
        Self {
            inner,
            _reset: reset_tasks,
        }
    }
}
#[async_trait]
impl StreamConnect for RtpMuxConnector {
    async fn connect(&self, addr: SocketAddr) -> io::Result<Box<dyn AsConn>> {
        self.inner
            .connect_stream(addr)
            .await
            .map(ProxyRtpMuxClientStream)
            .map(|stream| Box::new(stream) as Box<dyn AsConn>)
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
    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.0).poll_flush(cx)
    }
    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.0).poll_shutdown(cx)
    }
}
impl OwnIoStream for ProxyRtpMuxClientStream {}
impl AsConn for ProxyRtpMuxClientStream {}
impl HasIoAddr for ProxyRtpMuxClientStream {
    fn peer_addr(&self) -> io::Result<SocketAddr> {
        Ok(self.0.addr().peer_addr)
    }
    fn local_addr(&self) -> io::Result<SocketAddr> {
        Ok(self.0.addr().local_addr)
    }
}
