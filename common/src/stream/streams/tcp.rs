use std::{io, net::SocketAddr, sync::Arc};

use async_trait::async_trait;
use thiserror::Error;
use tokio::{
    net::{TcpListener, TcpStream},
    sync::mpsc,
};
use tracing::{error, info, instrument, trace};

use crate::{
    error::AnyResult,
    loading,
    stream::{ConnectStream, CreatedStream, IoAddr, IoStream, StreamServerHook},
};

#[derive(Debug)]
pub struct TcpServer<H> {
    listener: TcpListener,
    hook: H,
    set_hook_tx: mpsc::Sender<H>,
    set_hook_rx: mpsc::Receiver<H>,
}

impl<H> TcpServer<H> {
    pub fn new(listener: TcpListener, hook: H) -> Self {
        let (set_hook_tx, set_hook_rx) = mpsc::channel(64);
        Self {
            listener,
            hook,
            set_hook_tx,
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

#[async_trait]
impl<H> loading::Server for TcpServer<H>
where
    H: StreamServerHook + Send + Sync + 'static,
{
    type Hook = H;

    fn set_hook_tx(&self) -> &mpsc::Sender<Self::Hook> {
        &self.set_hook_tx
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
    pub async fn serve_(mut self) -> Result<(), ServeError> {
        drop(self.set_hook_tx);

        let addr = self.listener.local_addr().map_err(ServeError::LocalAddr)?;
        info!(?addr, "Listening");
        // Arc hook
        let mut hook = Arc::new(self.hook);
        loop {
            trace!("Waiting for connection");
            tokio::select! {
                res = self.listener.accept() => {
                    let (stream, _) = res.map_err(|e| ServeError::Accept { source: e, addr })?;
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
                    info!("Hook set");
                    hook = Arc::new(new_hook);
                }
            }
        }
        Ok(())
    }
}

#[derive(Debug, Error)]
pub enum ServeError {
    #[error("Failed to get local address")]
    LocalAddr(#[source] io::Error),
    #[error("Failed to accept connection")]
    Accept {
        #[source]
        source: io::Error,
        addr: SocketAddr,
    },
}

impl IoStream for TcpStream {}
impl IoAddr for TcpStream {
    fn peer_addr(&self) -> io::Result<SocketAddr> {
        self.peer_addr()
    }
    fn local_addr(&self) -> io::Result<SocketAddr> {
        self.local_addr()
    }
}

#[derive(Debug, Clone, Copy)]
pub struct TcpConnector;

#[async_trait]
impl ConnectStream for TcpConnector {
    async fn connect(&self, addr: SocketAddr) -> io::Result<CreatedStream> {
        let stream = TcpStream::connect(addr)
            .await
            .inspect_err(|e| error!(?e, ?addr, "Failed to connect to address"))?;
        Ok(CreatedStream::Tcp(stream))
    }
}
