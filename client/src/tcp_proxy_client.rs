use std::{
    net::SocketAddr,
    ops::{Deref, DerefMut},
};

use models::{
    read_header_async, write_header_async, ProxyProtocolError, RequestHeader, ResponseHeader,
};
use tokio::net::TcpStream;
use tracing::{error, instrument, trace};

impl TcpProxyStream {
    #[instrument(skip_all)]
    pub async fn establish(addresses: &[SocketAddr]) -> Result<TcpProxyStream, ProxyProtocolError> {
        let mut stream = TcpStream::connect(addresses[0])
            .await
            .inspect_err(|e| error!(?e, "Failed to connect to upstream address"))?;

        // Convert addresses to headers
        let headers = addresses[1..]
            .iter()
            .map(|addr| RequestHeader { upstream: *addr })
            .collect::<Vec<_>>();

        // Write headers to stream
        for header in headers {
            trace!(?header, "Writing header to stream");
            write_header_async(&mut stream, &header)
                .await
                .inspect_err(|e| error!(?e, "Failed to write header to stream"))?;
        }

        // Read response
        for address in addresses[..addresses.len() - 1].iter() {
            trace!(?address, "Reading response from upstream address");
            let resp: ResponseHeader = read_header_async(&mut stream)
                .await
                .inspect_err(|e| error!(?e, "Failed to read response from upstream address"))?;
            if let Err(mut err) = resp.result {
                err.source = *address;
                error!(?err, "Response was not successful");
                return Err(ProxyProtocolError::Response(err));
            }
        }

        // Return stream
        Ok(TcpProxyStream(stream))
    }
}

#[derive(Debug)]
pub struct TcpProxyStream(TcpStream);

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
