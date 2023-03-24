use std::{io, net::SocketAddr};

use async_trait::async_trait;
use client::tcp_proxy_client::TcpProxyStream;
use common::{
    error::ProxyProtocolError,
    header::ProxyConfig,
    tcp::{TcpServer, TcpServerHook},
};
use tokio::net::{TcpStream, ToSocketAddrs};
use tracing::{error, instrument};

pub struct TcpProxyAccess {
    proxy_configs: Vec<ProxyConfig>,
    destination: SocketAddr,
}

impl TcpProxyAccess {
    pub fn new(proxy_configs: Vec<ProxyConfig>, destination: SocketAddr) -> Self {
        Self {
            proxy_configs,
            destination,
        }
    }

    pub async fn build(
        self,
        listen_addr: impl ToSocketAddrs,
    ) -> io::Result<TcpServer<TcpProxyAccess>> {
        let tcp_listener = tokio::net::TcpListener::bind(listen_addr)
            .await
            .inspect_err(|e| error!(?e, "Failed to bind to listen address"))?;
        Ok(TcpServer::new(tcp_listener, self))
    }
}

#[async_trait]
impl TcpServerHook for TcpProxyAccess {
    #[instrument(skip(self, stream))]
    async fn handle_stream(&self, stream: &mut TcpStream) {
        let res = proxy(stream, &self.proxy_configs, &self.destination).await;
        if let Err(e) = res {
            error!(?e, "Failed to proxy");
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
