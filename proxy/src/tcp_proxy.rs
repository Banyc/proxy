use std::io;

use models::{read_header, ProxyProtocolError};
use tokio::net::{TcpListener, TcpStream};
use tracing::{error, info, instrument};

pub struct TcpProxy {
    listener: TcpListener,
}

impl TcpProxy {
    pub fn new(listener: TcpListener) -> Self {
        Self { listener }
    }

    #[instrument(skip_all)]
    pub async fn serve(self) -> io::Result<()> {
        info!(addr = ?self.listener.local_addr(), "Listening");
        loop {
            let (stream, _) = self.listener.accept().await?;
            info!(peer_addr = ?stream.peer_addr(), "Accepted connection");
            tokio::spawn(async move {
                if let Err(e) = proxy(stream).await {
                    error!(?e, "Error handling connection");
                }
            });
        }
    }
}

#[instrument(skip_all)]
async fn proxy(mut downstream: TcpStream) -> Result<(), ProxyProtocolError> {
    // Decode header
    let header = read_header(&mut downstream).await?;
    info!(?header, "Decoded header");

    // Connect to upstream
    let mut upstream = TcpStream::connect(header.upstream).await?;
    info!(peer_addr = ?upstream.peer_addr(), "Connected to upstream");

    // Copy data
    let (mut upstream_reader, mut upstream_writer) = upstream.split();
    let (mut downstream_reader, mut downstream_writer) = downstream.split();
    let upstream_to_downstream = tokio::io::copy(&mut upstream_reader, &mut downstream_writer);
    let downstream_to_upstream = tokio::io::copy(&mut downstream_reader, &mut upstream_writer);
    tokio::select! {
        res = upstream_to_downstream => res?,
        res = downstream_to_upstream => res?,
    };
    info!("Connection closed");

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use models::{write_header, Header};
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::TcpListener,
    };

    #[tokio::test]
    async fn test_proxy() {
        // Start proxy server
        let proxy_addr = {
            let listener = TcpListener::bind("localhost:0").await.unwrap();
            let proxy_addr = listener.local_addr().unwrap();
            let proxy = TcpProxy::new(listener);
            tokio::spawn(async move {
                proxy.serve().await.unwrap();
            });
            proxy_addr
        };

        // Message to send
        let req_msg = b"hello world";
        let resp_msg = b"goodbye world";

        // Start origin server
        let origin_addr = {
            let listener = TcpListener::bind("localhost:0").await.unwrap();
            let origin_addr = listener.local_addr().unwrap();
            tokio::spawn(async move {
                let (mut stream, _) = listener.accept().await.unwrap();
                let mut buf = [0; 1024];
                let mut msg_buf = &mut buf[..req_msg.len()];
                stream.read_exact(&mut msg_buf).await.unwrap();
                assert_eq!(msg_buf, req_msg);
                stream.write_all(resp_msg).await.unwrap();
            });
            origin_addr
        };

        // Connect to proxy server
        let mut stream = TcpStream::connect(proxy_addr).await.unwrap();

        // Establish connection to origin server
        {
            // Encode header
            let header = Header {
                upstream: origin_addr,
            };
            write_header(&mut stream, &header).await.unwrap();
        }

        // Write message
        stream.write_all(req_msg).await.unwrap();

        // Read response
        {
            let mut buf = [0; 1024];
            let mut msg_buf = &mut buf[..resp_msg.len()];
            stream.read_exact(&mut msg_buf).await.unwrap();
            assert_eq!(msg_buf, resp_msg);
        }
    }
}
