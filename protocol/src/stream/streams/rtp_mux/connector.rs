use async_trait::async_trait;
use common::{
    addr::any_addr,
    connect::{ConnectorConfig, ConnectorResetSignal},
    proto::connect::{
        stream::StreamConnect,
        udp::{UdpConnection, UdpMuxDialer},
    },
    stream::{ConnParts, HasIoAddr, OwnIoStream},
};
use mux::LaneClass;
use std::{
    io,
    net::SocketAddr,
    pin::Pin,
    sync::Arc,
    task::{Context, Poll},
};
use thiserror::Error;
use tokio::{
    io::AsyncWriteExt,
    io::{AsyncRead, AsyncWrite, ReadBuf},
};

use crate::stream::streams::mux::{STREAM_FLOW_KIND, UDP_FLOW_KIND};
#[derive(Debug)]
pub struct RtpMuxConnector {
    inner: Arc<::rtp_mux::RtpMuxConnector>,
}

/// A fatal error from a connector driver.
///
/// A connector driver is a process-lifetime task: it only exits when its
/// command loop or reset listener terminates, which leaves the connector
/// inert. Such an exit is fatal — the owning `server_tasks` `JoinSet` must
/// surface it so the server does not continue running with a dead
/// connector.
#[derive(Debug, Error)]
pub enum ConnectorDriverError {
    /// The inner [`rtp_mux::RtpMuxConnector`] command loop exited. It only
    /// returns when its command sender is dropped, i.e. every
    /// [`RtpMuxConnector`] handle has gone away — the connector is inert.
    #[error("rtp_mux connector command loop exited; connector is inert")]
    ConnectorExited,
    /// The proxy reset listener exited after a [`ConnectorResetSignal`]
    /// reset failed, i.e. the connector refused or failed a reset.
    #[error("rtp_mux connector reset listener exited after a failed reset")]
    ResetListenerExited,
}

/// The driver for a proxy [`RtpMuxConnector`].
///
/// It concurrently runs the inner [`rtp_mux::RtpMuxConnector`]'s connector
/// loop and the proxy reset listener that tears down sessions on a
/// [`ConnectorResetSignal`]. Spawn it into the parent runtime's
/// actively-reaped `JoinSet` so its exit is observed and its drop aborts
/// both child tasks.
///
/// Its [`Future::Output`] is a [`ConnectorDriverError`]: the driver only
/// exits when one of its children terminates, which is fatal — the
/// connector is left inert and must not continue serving.
#[must_use = "the connector is inert until the driver is spawned"]
pub struct RtpMuxConnectorDriver(
    Pin<Box<dyn std::future::Future<Output = ConnectorDriverError> + Send + 'static>>,
);

impl std::future::Future for RtpMuxConnectorDriver {
    type Output = ConnectorDriverError;
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
                () = inner_driver => ConnectorDriverError::ConnectorExited,
                () = reset_driver => ConnectorDriverError::ResetListenerExited,
            }
        }));
        (Self { inner }, driver)
    }
}
#[async_trait]
impl StreamConnect for RtpMuxConnector {
    async fn connect(&self, addr: SocketAddr) -> io::Result<Box<dyn ConnParts>> {
        let mut stream = self.inner.connect_stream(addr).await?;
        stream.write_all(&[STREAM_FLOW_KIND]).await?;
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
    async fn dial_udp(&self, addr: SocketAddr) -> io::Result<UdpConnection> {
        let mut stream = self
            .inner
            .connect_stream_with_lane(addr, LaneClass::Interactive)
            .await?;
        stream.write_all(&[UDP_FLOW_KIND]).await?;
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
