use std::{io, net::SocketAddr, sync::Arc};

use async_trait::async_trait;
use tokio::net::{TcpListener, TcpStream};
use tracing::{error, info, instrument, trace};

use super::{ConnectStream, CreatedStream, IoAddr, IoStream, StreamServerHook};

#[derive(Debug)]
pub struct TcpServer<H> {
    listener: TcpListener,
    hook: H,
}

impl<H> TcpServer<H> {
    pub fn new(listener: TcpListener, hook: H) -> Self {
        Self { listener, hook }
    }

    pub fn listener(&self) -> &TcpListener {
        &self.listener
    }

    pub fn listener_mut(&mut self) -> &mut TcpListener {
        &mut self.listener
    }
}

impl<H> TcpServer<H>
where
    H: StreamServerHook + Send + Sync + 'static,
{
    #[instrument(skip(self))]
    pub async fn serve(self) -> io::Result<()> {
        let addr = self
            .listener
            .local_addr()
            .inspect_err(|e| error!(?e, "Failed to get local address"))?;
        info!(?addr, "Listening");
        // Arc hook
        let hook = Arc::new(self.hook);
        loop {
            trace!("Waiting for connection");
            let (stream, _) = self
                .listener
                .accept()
                .await
                .inspect_err(|e| error!(?e, "Failed to accept connection"))?;
            // Arc hook
            let hook = Arc::clone(&hook);
            tokio::spawn(async move {
                hook.handle_stream(stream).await;
            });
        }
    }
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

#[derive(Debug)]
pub struct ConnectTcp;

#[async_trait]
impl ConnectStream for ConnectTcp {
    async fn connect(&self, addr: SocketAddr) -> io::Result<CreatedStream> {
        let stream = TcpStream::connect(addr)
            .await
            .inspect_err(|e| error!(?e, ?addr, "Failed to connect to address"))?;
        Ok(CreatedStream::Tcp(stream))
    }
}
