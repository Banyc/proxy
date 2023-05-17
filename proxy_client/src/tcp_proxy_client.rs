use std::ops::{Deref, DerefMut};

use common::{
    crypto::XorCryptoCursor,
    error::ProxyProtocolError,
    header::{
        convert_proxy_configs_to_header_crypto_pairs, read_header_async, write_header_async,
        InternetAddr, ProxyConfig, ResponseHeader,
    },
};
use tokio::net::TcpStream;
use tracing::{error, instrument, trace};

#[derive(Debug)]
pub struct TcpProxyStream(TcpStream);

impl TcpProxyStream {
    #[instrument(skip(proxy_configs))]
    pub async fn establish(
        proxy_configs: &[ProxyConfig],
        destination: &InternetAddr,
    ) -> Result<TcpProxyStream, ProxyProtocolError> {
        // If there are no proxy configs, just connect to the destination
        if proxy_configs.is_empty() {
            let addr = destination.to_socket_addr().await.inspect_err(|e| {
                error!(?e, ?destination, "Failed to resolve destination address")
            })?;
            let stream = TcpStream::connect(addr).await.inspect_err(|e| {
                error!(?e, ?destination, "Failed to connect to upstream address")
            })?;
            return Ok(TcpProxyStream(stream));
        }

        // Connect to the first proxy
        let proxy_addr = &proxy_configs[0].address;
        let addr = proxy_addr
            .to_socket_addr()
            .await
            .inspect_err(|e| error!(?e, ?proxy_addr, "Failed to resolve proxy address"))?;
        let mut stream = TcpStream::connect(addr)
            .await
            .inspect_err(|e| error!(?e, ?proxy_addr, "Failed to connect to upstream address"))?;

        // Convert addresses to headers
        let pairs = convert_proxy_configs_to_header_crypto_pairs(proxy_configs, destination);

        // Write headers to stream
        for (header, crypto) in pairs {
            trace!(?header, "Writing header to stream");
            let mut crypto_cursor = XorCryptoCursor::new(crypto);
            write_header_async(&mut stream, &header, &mut crypto_cursor)
                .await
                .inspect_err(|e| error!(?e, ?proxy_addr, "Failed to write header to stream"))?;
        }

        // Read response
        for node in proxy_configs {
            trace!(?node.address, "Reading response from upstream address");
            let mut crypto_cursor = XorCryptoCursor::new(&node.crypto);
            let resp: ResponseHeader = read_header_async(&mut stream, &mut crypto_cursor)
                .await
                .inspect_err(|e| {
                    error!(
                        ?e,
                        ?proxy_addr,
                        "Failed to read response from upstream address"
                    )
                })?;
            if let Err(mut err) = resp.result {
                err.source = node.address.clone();
                error!(?err, ?proxy_addr, "Response was not successful");
                return Err(ProxyProtocolError::Response(err));
            }
        }

        // Return stream
        Ok(TcpProxyStream(stream))
    }
}

impl Deref for TcpProxyStream {
    type Target = TcpStream;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl DerefMut for TcpProxyStream {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

impl TcpProxyStream {
    pub fn into_inner(self) -> TcpStream {
        self.0
    }
}
