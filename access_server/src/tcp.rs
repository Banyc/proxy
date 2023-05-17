use std::io;

use async_trait::async_trait;
use common::{
    crypto::{XorCrypto, XorCryptoCursor},
    error::ProxyProtocolError,
    header::{InternetAddr, ProxyConfig},
    tcp::{TcpServer, TcpServerHook, TcpXorStream},
};
use proxy_client::tcp_proxy_client::TcpProxyStream;
use tokio::net::{TcpStream, ToSocketAddrs};
use tracing::{error, instrument};

pub struct TcpProxyAccess {
    proxy_configs: Vec<ProxyConfig>,
    destination: InternetAddr,
    payload_crypto: Option<XorCrypto>,
}

impl TcpProxyAccess {
    pub fn new(
        proxy_configs: Vec<ProxyConfig>,
        destination: InternetAddr,
        payload_crypto: Option<XorCrypto>,
    ) -> Self {
        Self {
            proxy_configs,
            destination,
            payload_crypto,
        }
    }

    pub async fn build(self, listen_addr: impl ToSocketAddrs) -> io::Result<TcpServer<Self>> {
        let tcp_listener = tokio::net::TcpListener::bind(listen_addr)
            .await
            .inspect_err(|e| error!(?e, "Failed to bind to listen address"))?;
        Ok(TcpServer::new(tcp_listener, self))
    }

    async fn proxy(&self, downstream: &mut TcpStream) -> Result<(), ProxyProtocolError> {
        let upstream = TcpProxyStream::establish(&self.proxy_configs, &self.destination).await?;
        let mut upstream = upstream.into_inner();

        let res = match &self.payload_crypto {
            Some(crypto) => {
                // Establish encrypted stream
                let read_crypto_cursor = XorCryptoCursor::new(crypto);
                let write_crypto_cursor = XorCryptoCursor::new(crypto);
                let mut xor_stream =
                    TcpXorStream::new(upstream, read_crypto_cursor, write_crypto_cursor);
                tokio::io::copy_bidirectional(downstream, &mut xor_stream).await
            }
            None => tokio::io::copy_bidirectional(downstream, &mut upstream).await,
        };
        res?;

        Ok(())
    }
}

#[async_trait]
impl TcpServerHook for TcpProxyAccess {
    #[instrument(skip(self, stream))]
    async fn handle_stream(&self, mut stream: TcpStream) {
        let res = self.proxy(&mut stream).await;
        if let Err(e) = res {
            error!(?e, "Failed to proxy");
        }
    }
}
