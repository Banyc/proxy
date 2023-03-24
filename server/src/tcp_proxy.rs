use std::{io, net::SocketAddr};

use async_trait::async_trait;
use common::{
    error::{ProxyProtocolError, ResponseError, ResponseErrorKind},
    header::{read_header_async, write_header_async, RequestHeader, ResponseHeader, XorCrypto},
    tcp::{TcpServer, TcpServerHook},
};
use tokio::net::{TcpListener, TcpStream, ToSocketAddrs};
use tracing::{error, info, instrument};

pub struct TcpProxy {
    crypto: XorCrypto,
}

impl TcpProxy {
    pub fn new(crypto: XorCrypto) -> Self {
        Self { crypto }
    }

    pub async fn build(self, listen_addr: impl ToSocketAddrs) -> io::Result<TcpServer<TcpProxy>> {
        let listener = TcpListener::bind(listen_addr)
            .await
            .inspect_err(|e| error!(?e, "Failed to bind to listen address"))?;
        let server = TcpServer::new(listener, self);
        Ok(server)
    }
}

#[async_trait]
impl TcpServerHook for TcpProxy {
    #[instrument(skip(self, stream))]
    async fn handle_stream(&self, stream: &mut TcpStream) {
        let res = proxy(stream, &self.crypto).await;
        handle_proxy_result(stream, res, &self.crypto).await;
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
async fn proxy(
    downstream: &mut TcpStream,
    crypto: &XorCrypto,
) -> Result<StreamMetrics, ProxyProtocolError> {
    let downstream_addr = downstream
        .peer_addr()
        .inspect_err(|e| error!(?e, "Failed to get downstream address"))?;
    let start = std::time::Instant::now();

    // Decode header
    let header: RequestHeader = read_header_async(downstream, crypto)
        .await
        .inspect_err(|e| error!(?e, "Failed to read header from downstream"))?;

    // Prevent connections to localhost
    if header.upstream.ip().is_loopback() {
        error!(?header.upstream, "Refusing to connect to loopback address");
        return Err(ProxyProtocolError::Loopback);
    }

    // Connect to upstream
    let mut upstream = TcpStream::connect(header.upstream)
        .await
        .inspect_err(|e| error!(?e, ?header.upstream, "Failed to connect to upstream"))?;
    let upstream_addr = upstream
        .peer_addr()
        .inspect_err(|e| error!(?e, ?header.upstream, "Failed to get upstream address"))?;

    // Write Ok response
    let resp = ResponseHeader { result: Ok(()) };
    write_header_async(downstream, &resp, crypto)
        .await
        .inspect_err(|e| error!(?e, ?header.upstream, "Failed to write response to downstream"))?;

    // Copy data
    let (bytes_uplink, bytes_downlink) = tokio::io::copy_bidirectional(downstream, &mut upstream)
        .await
        .inspect_err(|e| error!(?e, ?header.upstream, "Failed to copy data between streams"))?;

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

#[instrument(skip(stream, res, crypto))]
async fn handle_proxy_result(
    stream: &mut TcpStream,
    res: Result<StreamMetrics, ProxyProtocolError>,
    crypto: &XorCrypto,
) {
    match res {
        Ok(metrics) => info!(?metrics, "Connection closed normally"),
        Err(e) => {
            error!(?e, "Connection closed with error");
            let _ = respond_with_error(stream, e, crypto)
                .await
                .inspect_err(|e| {
                    error!(?e, "Failed to respond with error to downstream after error")
                });
        }
    }
}

#[instrument(skip(stream, crypto))]
async fn respond_with_error(
    stream: &mut TcpStream,
    error: ProxyProtocolError,
    crypto: &XorCrypto,
) -> Result<(), ProxyProtocolError> {
    let local_addr = stream
        .local_addr()
        .inspect_err(|e| error!(?e, "Failed to get local address"))?;

    // Respond with error
    let resp = match error {
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
    write_header_async(stream, &resp, crypto)
        .await
        .inspect_err(|e| error!(?e, "Failed to write response to downstream after error"))?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::TcpListener,
    };

    #[tokio::test]
    async fn test_proxy() {
        let crypto = XorCrypto::default();

        // Start proxy server
        let proxy_addr = {
            let proxy = TcpProxy::new(crypto.clone());
            let server = proxy.build("localhost:0").await.unwrap();
            let proxy_addr = server.listener().local_addr().unwrap();
            tokio::spawn(async move {
                server.serve().await.unwrap();
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
                let msg_buf = &mut buf[..req_msg.len()];
                stream.read_exact(msg_buf).await.unwrap();
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
            write_header_async(&mut stream, &header, &crypto)
                .await
                .unwrap();

            // Read response
            let resp: ResponseHeader = read_header_async(&mut stream, &crypto).await.unwrap();
            assert!(resp.result.is_ok());
        }

        // Write message
        stream.write_all(req_msg).await.unwrap();

        // Read response
        {
            let mut buf = [0; 1024];
            let msg_buf = &mut buf[..resp_msg.len()];
            stream.read_exact(msg_buf).await.unwrap();
            assert_eq!(msg_buf, resp_msg);
        }
    }
}
