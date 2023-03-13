use std::net::SocketAddr;

use models::{write_header, Header, ProxyProtocolError};
use tokio::net::TcpStream;
use tracing::instrument;

pub struct TcpProxyClient {
    stream: TcpStream,
}

impl TcpProxyClient {
    pub fn new(stream: TcpStream) -> Self {
        Self { stream }
    }

    #[instrument(skip_all)]
    pub async fn establish(
        mut self,
        addresses: &[SocketAddr],
    ) -> Result<TcpStream, ProxyProtocolError> {
        // Convert addresses to headers
        let headers = addresses
            .iter()
            .map(|addr| Header {
                upstream: addr.clone(),
            })
            .collect::<Vec<_>>();

        // Write headers to stream
        for header in headers {
            write_header(&mut self.stream, &header).await?;
        }

        // Return stream
        Ok(self.stream)
    }
}
