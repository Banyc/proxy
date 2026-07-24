pub use ::rtp_mux::ServeError;
use common::{
    error::AnyResult,
    loading,
    stream::{AsConn, HasIoAddr, OwnIoStream, StreamServerHandleConn},
};
use std::{
    io,
    net::SocketAddr,
    pin::Pin,
    sync::{Arc, RwLock},
    task::{Context, Poll},
};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
#[derive(Debug)]
pub struct RtpMuxServer<ConnHandler> {
    inner: ::rtp_mux::RtpMuxServer,
    conn_handler: ConnHandler,
}
impl<ConnHandler> RtpMuxServer<ConnHandler> {
    pub fn new(
        interactive_listener: rtp::udp::Listener,
        bulk_listener: rtp::udp::Listener,
        conn_handler: ConnHandler,
        fec: bool,
    ) -> Self {
        Self::from_core(
            ::rtp_mux::RtpMuxServer::new(interactive_listener, bulk_listener, fec),
            conn_handler,
        )
    }
    pub(crate) fn from_core(inner: ::rtp_mux::RtpMuxServer, conn_handler: ConnHandler) -> Self {
        Self {
            inner,
            conn_handler,
        }
    }
    pub fn listener(&self) -> &rtp::udp::Listener {
        self.inner.listener()
    }
}
impl<ConnHandler> loading::Serve for RtpMuxServer<ConnHandler>
where
    ConnHandler: StreamServerHandleConn + Send + Sync + 'static,
{
    type ConnHandler = ConnHandler;
    async fn serve(
        self,
        mut set_conn_handler_rx: loading::ReplaceConnHandlerRx<Self::ConnHandler>,
    ) -> AnyResult {
        let Self {
            inner,
            conn_handler,
        } = self;
        let conn_handler = Arc::new(RwLock::new(Arc::new(conn_handler)));
        let handler_for_stream = Arc::clone(&conn_handler);
        let serving = inner.serve(move |stream| {
            let handler = handler_for_stream.read().unwrap().clone();
            tokio::spawn(async move {
                handler.handle_stream(ProxyRtpMuxStream(stream)).await;
            });
        });
        tokio::pin!(serving);
        loop {
            tokio::select! { result = &mut serving => return result.map_err(Into::into), replacement = set_conn_handler_rx.0.recv() => { let Some(replacement) = replacement else { return Ok(()); }; *conn_handler.write().unwrap() = Arc::new(replacement); } }
        }
    }
}
#[derive(Debug)]
struct ProxyRtpMuxStream(::rtp_mux::ServerStream);
impl AsyncRead for ProxyRtpMuxStream {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        Pin::new(&mut self.0).poll_read(cx, buf)
    }
}
impl AsyncWrite for ProxyRtpMuxStream {
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
impl OwnIoStream for ProxyRtpMuxStream {}
impl AsConn for ProxyRtpMuxStream {}
impl HasIoAddr for ProxyRtpMuxStream {
    fn peer_addr(&self) -> io::Result<SocketAddr> {
        Ok(self.0.addr().peer_addr)
    }
    fn local_addr(&self) -> io::Result<SocketAddr> {
        Ok(self.0.addr().local_addr)
    }
}
