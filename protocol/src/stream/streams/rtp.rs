use std::{io, net::SocketAddr, pin::Pin, sync::Arc};

use metrics::counter;
use thiserror::Error;
use tokio::{
    io::{AsyncRead, AsyncWrite},
    sync::mpsc,
};
use tracing::{error, info, instrument, trace, warn};

use common::{
    addr::any_addr,
    error::AnyResult,
    loading,
    stream::{connect::StreamConnect, IoAddr, IoStream, StreamServerHook},
};

use crate::stream::connection::Connection;

#[derive(Debug)]
pub struct RtpServer<H> {
    listener: rtp::udp::Listener,
    hook: H,
    handle: mpsc::Sender<H>,
    set_hook_rx: mpsc::Receiver<H>,
}

impl<H> RtpServer<H> {
    pub fn new(listener: rtp::udp::Listener, hook: H) -> Self {
        let (set_hook_tx, set_hook_rx) = mpsc::channel(64);
        Self {
            listener,
            hook,
            handle: set_hook_tx,
            set_hook_rx,
        }
    }

    pub fn listener(&self) -> &rtp::udp::Listener {
        &self.listener
    }

    pub fn listener_mut(&mut self) -> &mut rtp::udp::Listener {
        &mut self.listener
    }
}

impl<H> loading::Server for RtpServer<H>
where
    H: StreamServerHook + Send + Sync + 'static,
{
    type Hook = H;

    fn handle(&self) -> mpsc::Sender<Self::Hook> {
        self.handle.clone()
    }

    async fn serve(self) -> AnyResult {
        self.serve_().await.map_err(|e| e.into())
    }
}

impl<H> RtpServer<H>
where
    H: StreamServerHook + Send + Sync + 'static,
{
    #[instrument(skip(self))]
    async fn serve_(mut self) -> Result<(), ServeError> {
        drop(self.handle);

        let addr = self.listener.local_addr();
        info!(?addr, "Listening");
        // Arc hook
        let mut hook = Arc::new(self.hook);
        loop {
            trace!("Waiting for connection");
            tokio::select! {
                res = self.listener.accept_without_handshake() => {
                    let accepted = match res {
                        Ok(res) => res,
                        Err(e) => {
                            warn!(?e, ?addr, "Accept error");
                            continue;
                        }
                    };
                    let stream = AddressedRtpStream {
                        read: accepted.read.into_async_read(),
                        write: accepted.write.into_async_write(),
                        local_addr: self.listener.local_addr(),
                        peer_addr: accepted.peer_addr,
                    };
                    counter!("stream.rtp.accepts").increment(1);
                    // Arc hook
                    let hook = Arc::clone(&hook);
                    tokio::spawn(async move {
                        hook.handle_stream(stream).await;
                    });
                }
                res = self.set_hook_rx.recv() => {
                    let new_hook = match res {
                        Some(new_hook) => new_hook,
                        None => break,
                    };
                    info!(?addr, "Hook set");
                    hook = Arc::new(new_hook);
                }
            }
        }
        Ok(())
    }
}

#[derive(Debug, Error)]
pub enum ServeError {
    #[error("Failed to accept connection: {source}, {addr}")]
    Accept {
        #[source]
        source: io::Error,
        addr: SocketAddr,
    },
}

#[derive(Debug, Clone)]
pub struct RtpConnector;

impl StreamConnect for RtpConnector {
    type Connection = Connection;
    async fn connect(&self, addr: SocketAddr) -> io::Result<Connection> {
        let connected =
            rtp::udp::connect_without_handshake(any_addr(&addr.ip()), addr, None).await?;
        let stream = AddressedRtpStream {
            read: connected.read.into_async_read(),
            write: connected.write.into_async_write(),
            local_addr: connected.local_addr,
            peer_addr: connected.peer_addr,
        };
        counter!("stream.rtp.connects").increment(1);
        Ok(Connection::Rtp(stream))
    }
}

#[derive(Debug)]
pub struct AddressedRtpStream {
    read: rtp::socket::ReadStream,
    write: rtp::socket::WriteStream,
    local_addr: SocketAddr,
    peer_addr: SocketAddr,
}
impl IoStream for AddressedRtpStream {}
impl IoAddr for AddressedRtpStream {
    fn peer_addr(&self) -> io::Result<SocketAddr> {
        Ok(self.peer_addr)
    }
    fn local_addr(&self) -> io::Result<SocketAddr> {
        Ok(self.local_addr)
    }
}
impl AsyncRead for AddressedRtpStream {
    fn poll_read(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<io::Result<()>> {
        Pin::new(&mut self.read).poll_read(cx, buf)
    }
}
impl AsyncWrite for AddressedRtpStream {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> std::task::Poll<Result<usize, io::Error>> {
        Pin::new(&mut self.write).poll_write(cx, buf)
    }

    fn poll_flush(
        mut self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), io::Error>> {
        Pin::new(&mut self.write).poll_flush(cx)
    }

    fn poll_shutdown(
        mut self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), io::Error>> {
        Pin::new(&mut self.write).poll_shutdown(cx)
    }
}
