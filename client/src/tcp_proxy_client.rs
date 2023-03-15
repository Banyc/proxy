use std::net::SocketAddr;

use models::{read_header, write_header, ProxyProtocolError, RequestHeader, ResponseHeader};
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
    ) -> Result<TcpProxyStream, ProxyProtocolError> {
        // Convert addresses to headers
        let headers = addresses
            .iter()
            .map(|addr| RequestHeader {
                upstream: addr.clone(),
            })
            .collect::<Vec<_>>();

        // Write headers to stream
        for header in headers {
            write_header(&mut self.stream, &header).await?;
        }

        // Read response
        let resp: ResponseHeader = read_header(&mut self.stream).await?;
        if let Err(err) = resp.result {
            return Err(ProxyProtocolError::Response(err));
        }

        // Return stream
        Ok(TcpProxyStream(self.stream))
    }
}

pub struct TcpProxyStream(pub TcpStream);
