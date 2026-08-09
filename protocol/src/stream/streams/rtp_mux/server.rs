pub use ::rtp_mux::ServeError;
use common::{
    error::AnyResult,
    loading,
    session::{SessionSpawner, log_rejection},
    stream::{ConnParts, HasIoAddr, OwnIoStream},
};
use std::{
    io,
    net::SocketAddr,
    pin::Pin,
    sync::{Arc, RwLock},
    task::{Context, Poll},
};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

use crate::stream::streams::mux::{MuxProxyConnHandler, SocketAddrPair, dispatch_mux_flow};

#[derive(Debug)]
pub struct RtpMuxServer<ConnHandler> {
    inner: ::rtp_mux::RtpMuxServer,
    conn_handler: ConnHandler,
    session_spawner: SessionSpawner,
}
impl<ConnHandler> RtpMuxServer<ConnHandler> {
    pub fn new(
        interactive_listener: rtp::udp::Listener,
        bulk_listener: rtp::udp::Listener,
        conn_handler: ConnHandler,
        fec: bool,
        session_spawner: SessionSpawner,
    ) -> Self {
        Self::from_core(
            ::rtp_mux::RtpMuxServer::new(interactive_listener, bulk_listener, fec),
            conn_handler,
            session_spawner,
        )
    }
    pub(crate) fn from_core(
        inner: ::rtp_mux::RtpMuxServer,
        conn_handler: ConnHandler,
        session_spawner: SessionSpawner,
    ) -> Self {
        Self {
            inner,
            conn_handler,
            session_spawner,
        }
    }
    pub fn listener(&self) -> &rtp::udp::Listener {
        self.inner.listener()
    }
}
impl<ConnHandler> loading::Serve for RtpMuxServer<ConnHandler>
where
    ConnHandler: MuxProxyConnHandler,
{
    type ConnHandler = ConnHandler;
    async fn serve(
        self,
        mut set_conn_handler_rx: loading::ReplaceConnHandlerRx<Self::ConnHandler>,
    ) -> AnyResult {
        let Self {
            inner,
            conn_handler,
            session_spawner,
        } = self;
        let conn_handler = Arc::new(RwLock::new(Arc::new(conn_handler)));
        let handler_for_stream = Arc::clone(&conn_handler);
        let mux_session_spawner = rtp_mux::SessionSpawner::new({
            let session_spawner = session_spawner.clone();
            move |session| match session_spawner.try_spawn(async move {
                session.await;
                Ok(())
            }) {
                Ok(()) => {}
                Err(error) => log_rejection("rtp_mux_session", error),
            }
        });
        let serving = inner.serve(mux_session_spawner, move |stream| {
            let handler = handler_for_stream.read().unwrap().clone();
            let session_spawner = session_spawner.clone();
            match session_spawner.try_spawn(async move {
                let addr: SocketAddrPair = stream.addr().into();
                dispatch_mux_flow(
                    stream,
                    addr,
                    handler,
                    |stream, _addr| ProxyRtpMuxStream(stream),
                    "stream.rtp_mux.udp.flows",
                )
                .await;
                Ok(())
            }) {
                Ok(()) => {}
                Err(error) => log_rejection("rtp_mux_stream", error),
            }
        });
        tokio::pin!(serving);
        loop {
            tokio::select! {
                result = &mut serving => return result.map_err(Into::into),
                replacement = set_conn_handler_rx.recv() => {
                    match replacement {
                        Ok(Some(new_handler)) => {
                            *conn_handler.write().unwrap() = new_handler;
                        }
                        Ok(None) => {}
                        Err(()) => return Ok(()),
                    }
                }
            }
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
impl ConnParts for ProxyRtpMuxStream {}
impl HasIoAddr for ProxyRtpMuxStream {
    fn peer_addr(&self) -> io::Result<SocketAddr> {
        Ok(self.0.addr().peer_addr)
    }
    fn local_addr(&self) -> io::Result<SocketAddr> {
        Ok(self.0.addr().local_addr)
    }
}
