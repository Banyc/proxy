use std::{io, net::SocketAddr, sync::Arc};

use async_trait::async_trait;
use tokio::net::{TcpListener, TcpStream};
use tracing::{error, info, instrument, trace};

use crate::{error::ProxyProtocolError, header::InternetAddr};

use super::{pool::Pool, ConnectStream, CreatedStream, IoAddr, IoStream, StreamServerHook};

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
    type Stream = TcpStream;

    async fn connect(&self, addr: SocketAddr) -> io::Result<Self::Stream> {
        let stream = TcpStream::connect(addr)
            .await
            .inspect_err(|e| error!(?e, ?addr, "Failed to connect to address"))?;
        Ok(stream)
    }
}

pub async fn connect(
    addr: &InternetAddr,
    tcp_pool: &Pool,
    allow_loopback: bool,
) -> Result<(CreatedStream, SocketAddr), ProxyProtocolError> {
    let stream = tcp_pool.open_stream(addr).await;
    let ret = match stream {
        Some((stream, sock_addr)) => (stream, sock_addr),
        None => {
            let sock_addr = addr
                .to_socket_addr()
                .await
                .inspect_err(|e| error!(?e, ?addr, "Failed to resolve address"))?;
            if !allow_loopback && sock_addr.ip().is_loopback() {
                // Prevent connections to localhost
                error!(?addr, "Refusing to connect to loopback address");
                return Err(ProxyProtocolError::Loopback);
            }
            let stream = TcpStream::connect(sock_addr)
                .await
                .inspect_err(|e| error!(?e, ?addr, "Failed to connect to address"))?;
            (CreatedStream::Tcp(stream), sock_addr)
        }
    };
    Ok(ret)
}
