use std::{io, net::SocketAddr, pin::Pin, sync::Arc};

use async_trait::async_trait;
use thiserror::Error;
use tokio::{
    io::{AsyncRead, AsyncWrite},
    sync::mpsc,
};
use tokio_kcp::{KcpConfig, KcpListener, KcpNoDelayConfig, KcpStream};
use tracing::{error, info, instrument, trace};

use crate::{
    addr::any_addr,
    stream::{ConnectStream, CreatedStream, IoAddr, IoStream, StreamServerHook},
};

#[derive(Debug)]
pub struct KcpServer<H> {
    listener: KcpListener,
    hook: H,
    set_hook_tx: mpsc::Sender<H>,
    set_hook_rx: mpsc::Receiver<H>,
}

impl<H> KcpServer<H> {
    pub fn new(listener: KcpListener, hook: H) -> Self {
        let (set_hook_tx, set_hook_rx) = mpsc::channel(64);
        Self {
            listener,
            hook,
            set_hook_tx,
            set_hook_rx,
        }
    }

    pub fn listener(&self) -> &KcpListener {
        &self.listener
    }

    pub fn listener_mut(&mut self) -> &mut KcpListener {
        &mut self.listener
    }

    pub fn set_hook_tx(&self) -> &mpsc::Sender<H> {
        &self.set_hook_tx
    }
}

impl<H> KcpServer<H>
where
    H: StreamServerHook + Send + Sync + 'static,
{
    #[instrument(skip(self))]
    pub async fn serve(mut self) -> Result<(), ServeError> {
        drop(self.set_hook_tx);

        let addr = self.listener.local_addr().map_err(ServeError::LocalAddr)?;
        info!(?addr, "Listening");
        // Arc hook
        let mut hook = Arc::new(self.hook);
        loop {
            trace!("Waiting for connection");
            tokio::select! {
                res = self.listener.accept() => {
                    let (stream, peer_addr) = res.map_err(|e| ServeError::Accept { source: e.into(), addr })?;
                    let stream = AddressedKcpStream {
                        stream,
                        local_addr: addr,
                        peer_addr,
                    };
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

#[derive(Debug)]
pub struct AddressedKcpStream {
    stream: KcpStream,
    local_addr: SocketAddr,
    peer_addr: SocketAddr,
}

impl IoStream for AddressedKcpStream {}
impl IoAddr for AddressedKcpStream {
    fn peer_addr(&self) -> io::Result<SocketAddr> {
        Ok(self.peer_addr)
    }
    fn local_addr(&self) -> io::Result<SocketAddr> {
        Ok(self.local_addr)
    }
}

impl AsyncRead for AddressedKcpStream {
    fn poll_read(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<io::Result<()>> {
        Pin::new(&mut self.stream).poll_read(cx, buf)
    }
}

impl AsyncWrite for AddressedKcpStream {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> std::task::Poll<Result<usize, io::Error>> {
        Pin::new(&mut self.stream).poll_write(cx, buf)
    }

    fn poll_flush(
        mut self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), io::Error>> {
        Pin::new(&mut self.stream).poll_flush(cx)
    }

    fn poll_shutdown(
        mut self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), io::Error>> {
        Pin::new(&mut self.stream).poll_shutdown(cx)
    }
}

#[derive(Debug)]
pub struct KcpConnector;

#[async_trait]
impl ConnectStream for KcpConnector {
    async fn connect(&self, addr: SocketAddr) -> io::Result<CreatedStream> {
        let config = fast_kcp_config();
        let stream = KcpStream::connect(&config, addr)
            .await
            .inspect_err(|e| error!(?e, ?addr, "Failed to connect to address"))?;
        let local_addr = any_addr(&addr.ip());
        let stream = AddressedKcpStream {
            stream,
            local_addr,
            peer_addr: addr,
        };
        Ok(CreatedStream::Kcp(stream))
    }
}

pub fn fast_kcp_config() -> KcpConfig {
    KcpConfig {
        /* cSpell:disable */
        nodelay: KcpNoDelayConfig::fastest(),
        ..Default::default()
    }
}
