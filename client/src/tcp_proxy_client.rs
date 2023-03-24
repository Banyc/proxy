use std::{
    net::SocketAddr,
    ops::{Deref, DerefMut},
};

use common::{
    error::ProxyProtocolError,
    header::{
        convert_proxy_configs_to_header_crypto_pairs, read_header_async, write_header_async,
        ProxyConfig, ResponseHeader,
    },
};
use tokio::net::TcpStream;
use tracing::{error, instrument, trace};

#[derive(Debug)]
pub struct TcpProxyStream(TcpStream);

impl TcpProxyStream {
    #[instrument(skip_all)]
    pub async fn establish(
        proxy_configs: &[ProxyConfig],
        destination: &SocketAddr,
    ) -> Result<TcpProxyStream, ProxyProtocolError> {
        // If there are no proxy configs, just connect to the destination
        if proxy_configs.is_empty() {
            let stream = TcpStream::connect(destination)
                .await
                .inspect_err(|e| error!(?e, "Failed to connect to upstream address"))?;
            return Ok(TcpProxyStream(stream));
        }

        // Connect to the first upstream
        let mut stream = TcpStream::connect(proxy_configs[0].address)
            .await
            .inspect_err(|e| error!(?e, "Failed to connect to upstream address"))?;

        // Convert addresses to headers
        let pairs = convert_proxy_configs_to_header_crypto_pairs(proxy_configs, destination);

        // Write headers to stream
        for (header, crypto) in pairs {
            trace!(?header, "Writing header to stream");
            write_header_async(&mut stream, &header, crypto)
                .await
                .inspect_err(|e| error!(?e, "Failed to write header to stream"))?;
        }

        // Read response
        for node in proxy_configs {
            trace!(?node.address, "Reading response from upstream address");
            let resp: ResponseHeader = read_header_async(&mut stream, &node.crypto)
                .await
                .inspect_err(|e| error!(?e, "Failed to read response from upstream address"))?;
            if let Err(mut err) = resp.result {
                err.source = node.address;
                error!(?err, "Response was not successful");
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
