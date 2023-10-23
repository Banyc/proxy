use std::{io, net::SocketAddr, num::NonZeroUsize, sync::Arc};

use async_trait::async_trait;
use metrics::counter;
use mptcp::{listen::MptcpListener, stream::MptcpStream};
use thiserror::Error;
use tokio::sync::mpsc;
use tracing::{info, instrument, trace, warn};

use crate::{
    error::AnyResult,
    loading,
    stream::{connect::StreamConnect, CreatedStream, IoAddr, IoStream, StreamServerHook},
};

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

#[async_trait]
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
                    counter!("stream.mptcp.accepts", 1);
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
    #[error("Failed to get local address: {0}")]
    LocalAddr(#[source] io::Error),
    #[error("Failed to accept connection: {source}, {addr}")]
    Accept {
        #[source]
        source: io::Error,
        addr: SocketAddr,
    },
}

impl IoStream for MptcpStream {}
impl IoAddr for MptcpStream {
    fn peer_addr(&self) -> io::Result<SocketAddr> {
        Err(io::Error::new(io::ErrorKind::Unsupported, ""))
    }
    fn local_addr(&self) -> io::Result<SocketAddr> {
        Err(io::Error::new(io::ErrorKind::Unsupported, ""))
    }
}

#[derive(Debug, Clone, Copy)]
pub struct MptcpConnector;

#[async_trait]
impl StreamConnect for MptcpConnector {
    async fn connect(&self, addr: SocketAddr) -> io::Result<CreatedStream> {
        let stream = MptcpStream::connect(addr, NonZeroUsize::new(STREAMS).unwrap()).await?;
        counter!("stream.mptcp.connects", 1);
        Ok(CreatedStream::Mptcp(stream))
    }
}
