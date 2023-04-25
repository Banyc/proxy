use std::{fmt::Display, io, net::SocketAddr, sync::Arc};

use async_trait::async_trait;
use bytesize::ByteSize;
use tokio::net::{TcpListener, TcpStream};
use tracing::{error, info, instrument, trace};

use crate::header::InternetAddr;

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
    H: TcpServerHook + Send + Sync + 'static,
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

#[async_trait]
pub trait TcpServerHook {
    async fn handle_stream(&self, stream: TcpStream);
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct StreamMetrics {
    pub start: std::time::Instant,
    pub end: std::time::Instant,
    pub bytes_uplink: u64,
    pub bytes_downlink: u64,
    pub upstream_addr: InternetAddr,
    pub resolved_upstream_addr: SocketAddr,
    pub downstream_addr: SocketAddr,
}

impl Display for StreamMetrics {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let duration = self.end - self.start;
        let duration = duration.as_secs_f64();
        let uplink_speed = self.bytes_uplink as f64 / duration;
        let downlink_speed = self.bytes_downlink as f64 / duration;
        write!(
            f,
            "up: {{ {}, {}/s }}, down: {{ {}, {}/s }}, duration: {:.1} s, upstream: {{ {}, resolved: {} }}, downstream: {}",
            ByteSize::b(self.bytes_uplink),
            ByteSize::b(uplink_speed as u64),
            ByteSize::b(self.bytes_downlink),
            ByteSize::b(downlink_speed as u64),
            duration,
            self.upstream_addr,
            self.resolved_upstream_addr,
            self.downstream_addr
        )
    }
}
