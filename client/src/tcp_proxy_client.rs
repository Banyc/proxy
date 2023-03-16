use std::{
    net::SocketAddr,
    ops::{Deref, DerefMut},
};

use models::{read_header, write_header, ProxyProtocolError, RequestHeader, ResponseHeader};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpStream,
};
use tracing::{instrument, trace};

impl TcpProxyStream {
    #[instrument(skip_all)]
    pub async fn establish(addresses: &[SocketAddr]) -> Result<TcpProxyStream, ProxyProtocolError> {
        let mut stream = TcpStream::connect(addresses[0]).await?;

        // Convert addresses to headers
        let headers = addresses[1..]
            .iter()
            .map(|addr| RequestHeader { upstream: *addr })
            .collect::<Vec<_>>();

        // Write headers to stream
        for header in headers {
            write_header(&mut stream, &header).await?;
            trace!(?header, "Wrote header");
        }

        // Read response
        for address in addresses[..addresses.len() - 1].iter() {
            let resp: ResponseHeader = read_header(&mut stream).await?;
            trace!(?resp, "Read response");
            if let Err(mut err) = resp.result {
                err.source = *address;
                return Err(ProxyProtocolError::Response(err));
            }
            trace!("Response was successful");
        }
        trace!("All responses were successful");

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

    pub async fn close_gracefully(mut self) -> Result<(), ProxyProtocolError> {
        // Shutdown write half
        self.0.shutdown().await?;
        // Read until EOF
        let mut buf = [0; 1024];
        while self.0.read(&mut buf).await? > 0 {}
        Ok(())
    }
}
