use std::{io, net::SocketAddr, pin::Pin, sync::Arc};

use quinn::{Endpoint, RecvStream, SendStream};
use tokio::io::{AsyncRead, AsyncWrite};

use crate::stream::{IoAddr, IoStream, StreamServerHook};

#[derive(Debug)]
pub struct QuicServer<H> {
    listener: Endpoint,
    hook: H,
}
impl<H> QuicServer<H> {
    pub fn new(listener: Endpoint, hook: H) -> Self {
        Self { listener, hook }
    }
}
impl<H> QuicServer<H>
where
    H: StreamServerHook + Send + Sync + 'static,
{
    pub async fn serve(self) -> io::Result<()> {
        let hook = Arc::new(self.hook);
        loop {
            let local_addr = self.listener.local_addr()?;
            let incoming_conn = self.listener.accept().await.unwrap();
            let conn = incoming_conn.await?;
            let hook = Arc::clone(&hook);
            tokio::spawn(async move {
                loop {
                    let (send, recv) = conn.accept_bi().await?;
                    let stream = QuicIoStream {
                        send,
                        recv,
                        peer_addr: conn.remote_address(),
                        local_addr,
                    };

                    let hook = Arc::clone(&hook);
                    tokio::spawn(async move {
                        hook.handle_stream(stream).await;
                    });
                }
                #[allow(unreachable_code)]
                io::Result::Ok(())
            });
        }
    }
}

#[derive(Debug)]
pub struct QuicIoStream {
    send: SendStream,
    recv: RecvStream,
    peer_addr: SocketAddr,
    local_addr: SocketAddr,
}
impl IoAddr for QuicIoStream {
    fn peer_addr(&self) -> io::Result<SocketAddr> {
        Ok(self.peer_addr)
    }

    fn local_addr(&self) -> io::Result<SocketAddr> {
        Ok(self.local_addr)
    }
}
impl IoStream for QuicIoStream {}
impl AsyncRead for QuicIoStream {
    fn poll_read(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<io::Result<()>> {
        Pin::new(&mut self.recv).poll_read(cx, buf)
    }
}
impl AsyncWrite for QuicIoStream {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> std::task::Poll<Result<usize, io::Error>> {
        Pin::new(&mut self.send).poll_write(cx, buf)
    }

    fn poll_flush(
        mut self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), io::Error>> {
        Pin::new(&mut self.send).poll_flush(cx)
    }

    fn poll_shutdown(
        mut self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), io::Error>> {
        Pin::new(&mut self.send).poll_shutdown(cx)
    }
}
