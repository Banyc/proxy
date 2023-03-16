use std::{io, net::SocketAddr};

use models::{
    read_header, write_header, ProxyProtocolError, RequestHeader, ResponseError, ResponseErrorKind,
    ResponseHeader,
};
use tokio::net::{TcpListener, TcpStream};
use tracing::{error, info, instrument, trace};

pub struct TcpProxy {
    listener: TcpListener,
}

impl TcpProxy {
    pub fn new(listener: TcpListener) -> Self {
        Self { listener }
    }

    #[instrument(skip_all)]
    pub async fn serve(self) -> io::Result<()> {
        let addr = self.listener.local_addr()?;
        info!(?addr, "Listening");
        loop {
            let (stream, _) = self.listener.accept().await?;
            trace!(peer_addr = ?stream.peer_addr(), "Accepted connection");
            tokio::spawn(async move {
                let mut stream = stream;
                let res = proxy(&mut stream).await;
                teardown(&mut stream, res).await;
            });
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct StreamMetrics {
    start: std::time::Instant,
    end: std::time::Instant,
    bytes_uplink: u64,
    bytes_downlink: u64,
    upstream_addr: SocketAddr,
    downstream_addr: SocketAddr,
}

#[instrument(skip_all)]
async fn proxy(downstream: &mut TcpStream) -> Result<StreamMetrics, ProxyProtocolError> {
    let downstream_addr = downstream.peer_addr()?;
    trace!(?downstream_addr, "Connected to downstream");
    let start = std::time::Instant::now();

    // Decode header
    let header: RequestHeader = read_header(downstream).await?;
    trace!(?header, "Decoded header");

    // Prevent connections to localhost
    if header.upstream.ip().is_loopback() {
        return Err(ProxyProtocolError::Loopback);
    }
    trace!(?header.upstream, "Validated upstream address");

    // Connect to upstream
    let mut upstream = TcpStream::connect(header.upstream).await?;
    trace!(?header.upstream, "Connected to upstream");
    let upstream_addr = upstream.peer_addr()?;
    trace!(?upstream_addr, "Connected to upstream");

    // Write Ok response
    let resp = ResponseHeader { result: Ok(()) };
    write_header(downstream, &resp).await?;
    trace!(?resp, "Wrote response");

    // Copy data
    let (bytes_uplink, bytes_downlink) =
        tokio::io::copy_bidirectional(downstream, &mut upstream).await?;
    trace!(bytes_uplink, bytes_downlink, "Copied data");

    let end = std::time::Instant::now();
    let metrics = StreamMetrics {
        start,
        end,
        bytes_uplink,
        bytes_downlink,
        upstream_addr,
        downstream_addr,
    };
    Ok(metrics)
}

#[instrument(skip_all)]
async fn teardown(stream: &mut TcpStream, res: Result<StreamMetrics, ProxyProtocolError>) {
    match res {
        Ok(x) => info!(?x, "Connection closed normally"),
        Err(e) => {
            error!(?e, "Connection closed with error");

            let local_addr = stream.local_addr().unwrap();

            // Respond with error
            let resp = match e {
                ProxyProtocolError::Io(_) => ResponseHeader {
                    result: Err(ResponseError {
                        source: local_addr,
                        kind: ResponseErrorKind::Io,
                    }),
                },
                ProxyProtocolError::Bincode(_) => ResponseHeader {
                    result: Err(ResponseError {
                        source: local_addr,
                        kind: ResponseErrorKind::Codec,
                    }),
                },
                ProxyProtocolError::Loopback => ResponseHeader {
                    result: Err(ResponseError {
                        source: local_addr,
                        kind: ResponseErrorKind::Loopback,
                    }),
                },
                ProxyProtocolError::Response(err) => ResponseHeader { result: Err(err) },
            };
            let _ = write_header(stream, &resp).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use models::{write_header, RequestHeader};
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
            let listener = TcpListener::bind("[::]:0").await.unwrap();
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
            let header = RequestHeader {
                upstream: origin_addr,
            };
            write_header(&mut stream, &header).await.unwrap();

            // Read response
            let resp: ResponseHeader = read_header(&mut stream).await.unwrap();
            assert!(resp.result.is_ok());
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
