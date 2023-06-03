use std::{io, net::SocketAddr, sync::Arc, time::Duration};

use async_trait::async_trait;
use thiserror::Error;
use tokio::net::{TcpListener, TcpStream};
use tokio_io_timeout::TimeoutStream;
use tracing::{error, info, instrument, trace};

use crate::stream::{ConnectStream, CreatedStream, IoAddr, IoStream, StreamServerHook};

// const UPLINK_TIMEOUT: Duration = Duration::from_secs(60);
const DOWNLINK_TIMEOUT: Duration = Duration::from_secs(60);

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
    pub async fn serve(self) -> Result<(), ServeError> {
        let addr = self.listener.local_addr().map_err(ServeError::LocalAddr)?;
        info!(?addr, "Listening");
        // Arc hook
        let hook = Arc::new(self.hook);
        loop {
            trace!("Waiting for connection");
            let (stream, _) = self
                .listener
                .accept()
                .await
                .map_err(|e| ServeError::Accept { source: e, addr })?;
            let mut stream: TimeoutStream<TcpStream> = TimeoutStream::new(stream);
            // stream.set_read_timeout(Some(UPLINK_TIMEOUT));
            stream.set_write_timeout(Some(DOWNLINK_TIMEOUT));
            // Arc hook
            let hook = Arc::clone(&hook);
            tokio::spawn(async move {
                hook.handle_stream(CreatedStream::Tcp(Box::pin(stream)))
                    .await;
            });
        }
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
        let mut stream = TimeoutStream::new(stream);
        stream.set_read_timeout(Some(DOWNLINK_TIMEOUT));
        // stream.set_write_timeout(Some(UPLINK_TIMEOUT));
        Ok(CreatedStream::Tcp(Box::pin(stream)))
    }
}
