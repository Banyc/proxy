use std::io;

use async_trait::async_trait;
use common::{
    crypto::XorCrypto,
    error::ProxyProtocolError,
    header::{InternetAddr, ProxyConfig, ProxyConfigBuilder},
    stream::{pool::Pool, tcp::TcpServer, IoStream, StreamServerHook, XorStream},
};
use proxy_client::tcp_proxy_client::TcpProxyStream;
use serde::{Deserialize, Serialize};
use tokio::net::ToSocketAddrs;
use tracing::{error, instrument};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TcpProxyAccessBuilder {
    listen_addr: String,
    proxy_configs: Vec<ProxyConfigBuilder>,
    destination: String,
    payload_xor_key: Option<Vec<u8>>,
    tcp_pool: Option<Vec<String>>,
}

impl TcpProxyAccessBuilder {
    pub async fn build(self) -> io::Result<TcpServer<TcpProxyAccess>> {
        let tcp_pool = Pool::new();
        if let Some(addrs) = self.tcp_pool {
            tcp_pool.add_many_queues(addrs.into_iter().map(|v| v.into()));
        }
        let access = TcpProxyAccess::new(
            self.proxy_configs.into_iter().map(|x| x.build()).collect(),
            self.destination.into(),
            self.payload_xor_key.map(XorCrypto::new),
            tcp_pool,
        );
        let server = access.build(self.listen_addr).await?;
        Ok(server)
    }
}

pub struct TcpProxyAccess {
    proxy_configs: Vec<ProxyConfig>,
    destination: InternetAddr,
    payload_crypto: Option<XorCrypto>,
    tcp_pool: Pool,
}

impl TcpProxyAccess {
    pub fn new(
        proxy_configs: Vec<ProxyConfig>,
        destination: InternetAddr,
        payload_crypto: Option<XorCrypto>,
        tcp_pool: Pool,
    ) -> Self {
        Self {
            proxy_configs,
            destination,
            payload_crypto,
            tcp_pool,
        }
    }

    pub async fn build(self, listen_addr: impl ToSocketAddrs) -> io::Result<TcpServer<Self>> {
        let tcp_listener = tokio::net::TcpListener::bind(listen_addr)
            .await
            .inspect_err(|e| error!(?e, "Failed to bind to listen address"))?;
        Ok(TcpServer::new(tcp_listener, self))
    }

    async fn proxy<S>(&self, downstream: &mut S) -> Result<(), ProxyProtocolError>
    where
        S: IoStream,
    {
        let (mut upstream, _) =
            TcpProxyStream::establish(&self.proxy_configs, &self.destination, &self.tcp_pool)
                .await?;

        let res = match &self.payload_crypto {
            Some(crypto) => {
                // Establish encrypted stream
                let mut xor_stream = XorStream::upgrade(upstream, crypto);
                tokio::io::copy_bidirectional(downstream, &mut xor_stream).await
            }
            None => tokio::io::copy_bidirectional(downstream, &mut upstream).await,
        };
        res?;

        Ok(())
    }
}

#[async_trait]
impl StreamServerHook for TcpProxyAccess {
    #[instrument(skip(self, stream))]
    async fn handle_stream<S>(&self, mut stream: S)
    where
        S: IoStream,
    {
        let res = self.proxy(&mut stream).await;
        if let Err(e) = res {
            error!(?e, "Failed to proxy");
        }
    }
}
