use std::{io, net::SocketAddr, num::NonZeroUsize, sync::Arc};

use metrics::counter;
use mptcp::{listen::MptcpListener, stream::MptcpStream};
use thiserror::Error;
use tokio::{
    io::{AsyncRead, AsyncWrite},
    sync::mpsc,
};
use tracing::{info, instrument, trace, warn};

use common::{
    error::AnyResult,
    loading,
    stream::{connect::StreamConnect, IoAddr, IoStream, StreamServerHook},
};

use crate::stream::connection::Connection;

const STREAMS: usize = 4;

#[derive(Debug)]
pub struct MptcpServer<H> {
    listener: MptcpListener,
    hook: H,
    handle: mpsc::Sender<H>,
    set_hook_rx: mpsc::Receiver<H>,
}

impl<H> MptcpServer<H> {
    pub fn new(listener: MptcpListener, hook: H) -> Self {
        let (set_hook_tx, set_hook_rx) = mpsc::channel(64);
        Self {
            listener,
            hook,
            handle: set_hook_tx,
            set_hook_rx,
        }
    }

    pub fn listener(&self) -> &MptcpListener {
        &self.listener
    }

    pub fn listener_mut(&mut self) -> &mut MptcpListener {
        &mut self.listener
    }
}

impl<H> loading::Server for MptcpServer<H>
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

impl<H> MptcpServer<H>
where
    H: StreamServerHook + Send + Sync + 'static,
{
    #[instrument(skip(self))]
    async fn serve_(mut self) -> Result<(), ServeError> {
        drop(self.handle);

        let addr = self.listener.local_addr().map_err(ServeError::LocalAddr)?;
        info!(?addr, "Listening");
        // Arc hook
        let mut hook = Arc::new(self.hook);
        loop {
            trace!("Waiting for connection");
            tokio::select! {
                res = self.listener.accept() => {
                    let stream = match res {
                        Ok(res) => res,
                        Err(e) => {
                            warn!(?e, ?addr, "Accept error");
                            continue;
                        }
                    };
                    counter!("stream.mptcp.accepts").increment(1);
                    // Arc hook
                    let hook = Arc::clone(&hook);
                    tokio::spawn(async move {
                        hook.handle_stream(IoMptcpStream(stream)).await;
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
    #[error("Failed to get local address: {0}")]
    LocalAddr(#[source] io::Error),
    #[error("Failed to accept connection: {source}, {addr}")]
    Accept {
        #[source]
        source: io::Error,
        addr: SocketAddr,
    },
}

#[derive(Debug, Clone, Copy)]
pub struct MptcpConnector;

impl StreamConnect for MptcpConnector {
    type Connection = Connection;
    async fn connect(&self, addr: SocketAddr) -> io::Result<Connection> {
        let stream = MptcpStream::connect(addr, NonZeroUsize::new(STREAMS).unwrap()).await?;
        counter!("stream.mptcp.connects").increment(1);
        Ok(Connection::Mptcp(IoMptcpStream(stream)))
    }
}

#[derive(Debug)]
pub struct IoMptcpStream(pub MptcpStream);
impl AsyncWrite for IoMptcpStream {
    fn poll_write(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> std::task::Poll<Result<usize, io::Error>> {
        std::pin::Pin::new(&mut self.0).poll_write(cx, buf)
    }

    fn poll_flush(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), io::Error>> {
        std::pin::Pin::new(&mut self.0).poll_flush(cx)
    }

    fn poll_shutdown(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), io::Error>> {
        std::pin::Pin::new(&mut self.0).poll_shutdown(cx)
    }
}
impl AsyncRead for IoMptcpStream {
    fn poll_read(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<io::Result<()>> {
        std::pin::Pin::new(&mut self.0).poll_read(cx, buf)
    }
}
impl IoStream for IoMptcpStream {}
impl IoAddr for IoMptcpStream {
    fn peer_addr(&self) -> io::Result<SocketAddr> {
        self.0.peer_addr().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::Unsupported,
                "MptcpStream may not have a unified peer address",
            )
        })
    }
    fn local_addr(&self) -> io::Result<SocketAddr> {
        self.0.local_addr().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::Unsupported,
                "MptcpStream may not have a unified local address",
            )
        })
    }
}
