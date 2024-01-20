use std::{io, net::SocketAddr, pin::Pin, sync::Arc};

use metrics::counter;
use thiserror::Error;
use tokio::{
    io::{AsyncRead, AsyncWrite},
    net::{TcpListener, TcpStream},
    sync::mpsc,
};
use tracing::{info, instrument, trace, warn};

use common::{
    error::AnyResult,
    loading,
    stream::{connect::StreamConnect, IoAddr, IoStream, StreamServerHook},
};

use crate::stream::connection::Connection;

#[derive(Debug)]
pub struct TcpServer<H> {
    listener: TcpListener,
    hook: H,
    handle: mpsc::Sender<H>,
    set_hook_rx: mpsc::Receiver<H>,
}

impl<H> TcpServer<H> {
    pub fn new(listener: TcpListener, hook: H) -> Self {
        let (set_hook_tx, set_hook_rx) = mpsc::channel(64);
        Self {
            listener,
            hook,
            handle: set_hook_tx,
            set_hook_rx,
        }
    }

    pub fn listener(&self) -> &TcpListener {
        &self.listener
    }

    pub fn listener_mut(&mut self) -> &mut TcpListener {
        &mut self.listener
    }
}

impl<H> loading::Server for TcpServer<H>
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

impl<H> TcpServer<H>
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
                    // let (stream, _) = res.map_err(|e| ServeError::Accept { source: e, addr })?;
                    let (stream, _) = match res {
                        Ok(res) => res,
                        Err(e) => {
                            warn!(?e, ?addr, "Accept error");
                            continue;
                        }
                    };
                    counter!("stream.tcp.accepts").increment(1);
                    // Arc hook
                    let hook = Arc::clone(&hook);
                    tokio::spawn(async move {
                        hook.handle_stream(IoTcpStream(stream)).await;
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
pub struct TcpConnector;

impl StreamConnect for TcpConnector {
    type Connection = Connection;

    async fn connect(&self, addr: SocketAddr) -> io::Result<Connection> {
        let stream = TcpStream::connect(addr).await?;
        counter!("stream.tcp.connects").increment(1);
        Ok(Connection::Tcp(IoTcpStream(stream)))
    }
}

#[derive(Debug)]
pub struct IoTcpStream(pub TcpStream);
impl AsyncWrite for IoTcpStream {
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
impl AsyncRead for IoTcpStream {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<io::Result<()>> {
        std::pin::Pin::new(&mut self.0).poll_read(cx, buf)
    }
}
impl IoStream for IoTcpStream {}
impl IoAddr for IoTcpStream {
    fn peer_addr(&self) -> io::Result<SocketAddr> {
        self.0.peer_addr()
    }
    fn local_addr(&self) -> io::Result<SocketAddr> {
        self.0.local_addr()
    }
}
