use std::{io, net::SocketAddr, pin::Pin, sync::Arc};

use async_trait::async_trait;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio_kcp::{KcpConfig, KcpListener, KcpStream};
use tracing::{error, info, instrument, trace};

use super::{ConnectStream, CreatedStream, IoAddr, IoStream, StreamServerHook};

#[derive(Debug)]
pub struct KcpServer<H> {
    listener: KcpListener,
    hook: H,
}

impl<H> KcpServer<H> {
    pub fn new(listener: KcpListener, hook: H) -> Self {
        Self { listener, hook }
    }

    pub fn listener(&self) -> &KcpListener {
        &self.listener
    }

    pub fn listener_mut(&mut self) -> &mut KcpListener {
        &mut self.listener
    }
}

impl<H> KcpServer<H>
where
    H: StreamServerHook + Send + Sync + 'static,
{
    #[instrument(skip(self))]
    pub async fn serve(mut self) -> io::Result<()> {
        let addr = self
            .listener
            .local_addr()
            .inspect_err(|e| error!(?e, "Failed to get local address"))?;
        info!(?addr, "Listening");
        // Arc hook
        let hook = Arc::new(self.hook);
        loop {
            trace!("Waiting for connection");
            let (stream, peer_addr) = self
                .listener
                .accept()
                .await
                .inspect_err(|e| error!(?e, "Failed to accept connection"))?;
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
    }
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
        let config = KcpConfig::default();
        let stream = KcpStream::connect(&config, addr)
            .await
            .inspect_err(|e| error!(?e, ?addr, "Failed to connect to address"))?;
        let local_addr = match addr.ip() {
            std::net::IpAddr::V4(_) => "0.0.0.0:0".parse().unwrap(),
            std::net::IpAddr::V6(_) => "[::]:0".parse().unwrap(),
        };
        let stream = AddressedKcpStream {
            stream,
            local_addr,
            peer_addr: addr,
        };
        Ok(CreatedStream::Kcp(stream))
    }
}
