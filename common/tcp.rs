use std::{io, sync::Arc};

use async_trait::async_trait;
use tokio::net::{TcpListener, TcpStream};
use tracing::{error, info, instrument, trace};

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