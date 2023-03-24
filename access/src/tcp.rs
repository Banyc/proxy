use std::{io, net::SocketAddr, sync::Arc};

use client::tcp_proxy_client::TcpProxyStream;

use common::{error::ProxyProtocolError, header::ProxyConfig};
use tokio::net::{TcpListener, TcpStream};
use tracing::{error, info, instrument, trace};

pub struct TcpProxyAccess {
    listen_addr: SocketAddr,
    proxy_configs: Vec<ProxyConfig>,
    destination: SocketAddr,
}

impl TcpProxyAccess {
    pub fn new(
        listen_addr: SocketAddr,
        proxy_configs: Vec<ProxyConfig>,
        destination: SocketAddr,
    ) -> Self {
        Self {
            listen_addr,
            proxy_configs,
            destination,
        }
    }

    #[instrument(skip_all)]
    pub async fn serve(self) -> io::Result<()> {
        let listener = TcpListener::bind(self.listen_addr).await?;
        let addr = listener
            .local_addr()
            .inspect_err(|e| error!(?e, "Failed to get local address"))?;
        info!(?addr, "Listening");
        let proxy_config = Arc::new(self.proxy_configs);
        loop {
            trace!("Waiting for connection");
            let (stream, _) = listener
                .accept()
                .await
                .inspect_err(|e| error!(?e, "Failed to accept connection"))?;
            let proxy_configs = Arc::clone(&proxy_config);
            tokio::spawn(async move {
                let mut stream = stream;
                let res = proxy(&mut stream, &proxy_configs, &self.destination).await;
                if let Err(e) = res {
                    error!(?e, "Failed to proxy");
                }
            });
        }
    }
}

async fn proxy(
    downstream: &mut TcpStream,
    proxy_configs: &[ProxyConfig],
    destination: &SocketAddr,
) -> Result<(), ProxyProtocolError> {
    let upstream = TcpProxyStream::establish(proxy_configs, destination).await?;
    let mut upstream = upstream.into_inner();
    tokio::io::copy_bidirectional(downstream, &mut upstream).await?;

    Ok(())
}
