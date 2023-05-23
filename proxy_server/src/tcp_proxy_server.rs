use std::{collections::HashMap, io, net::SocketAddr, ops::DerefMut, pin::Pin};

use async_trait::async_trait;
use common::{
    crypto::{XorCrypto, XorCryptoCursor},
    error::{ProxyProtocolError, ResponseError, ResponseErrorKind},
    header::{read_header_async, write_header_async, InternetAddr, RequestHeader, ResponseHeader},
    stream::{tcp::TcpServer, IoAddr, IoStream, StreamMetrics, StreamServerHook, XorStream},
};
use quinn::{Connection, RecvStream, SendStream};
use serde::Deserialize;
use tokio::{
    io::{AsyncRead, AsyncWrite},
    net::{TcpListener, TcpStream, ToSocketAddrs},
};
use tracing::{error, info, instrument};

pub struct TcpProxyAcceptor {
    crypto: XorCrypto,
    quic: HashMap<InternetAddr, (Connection, SocketAddr)>,
}

impl TcpProxyAcceptor {
    pub fn new(crypto: XorCrypto, quic: HashMap<InternetAddr, (Connection, SocketAddr)>) -> Self {
        Self { crypto, quic }
    }

    #[instrument(skip(self, downstream))]
    async fn establish<S>(
        &self,
        downstream: &mut S,
    ) -> Result<(AcceptedStream, InternetAddr, SocketAddr), ProxyProtocolError>
    where
        S: IoStream + IoAddr,
    {
        // Decode header
        let mut read_crypto_cursor = XorCryptoCursor::new(&self.crypto);
        let header: RequestHeader = read_header_async(downstream, &mut read_crypto_cursor)
            .await
            .inspect_err(|e| {
                let downstream_addr = downstream.peer_addr().ok();
                error!(
                    ?e,
                    ?downstream_addr,
                    "Failed to read header from downstream"
                )
            })?;

        // Connect to upstream
        let quic = self.quic.get(&header.upstream);
        let (upstream, sock_addr) = match quic {
            Some((conn, sock_addr)) => {
                let (send, recv) = conn.open_bi().await.unwrap();
                (AcceptedStream::Quic { send, recv }, *sock_addr)
            }
            None => {
                // Prevent connections to localhost
                let upstream_sock_addr = header.upstream.to_socket_addr().await.inspect_err(
                    |e| error!(?e, ?header.upstream, "Failed to resolve upstream address"),
                )?;
                if upstream_sock_addr.ip().is_loopback() {
                    error!(?header.upstream, "Refusing to connect to loopback address");
                    return Err(ProxyProtocolError::Loopback);
                }

                let tcp_stream = TcpStream::connect(upstream_sock_addr).await.inspect_err(
                    |e| error!(?e, ?header.upstream, "Failed to connect to upstream"),
                )?;
                (AcceptedStream::Tcp(tcp_stream), upstream_sock_addr)
            }
        };

        // Write Ok response
        let resp = ResponseHeader { result: Ok(()) };
        let mut write_crypto_cursor = XorCryptoCursor::new(&self.crypto);
        write_header_async(downstream, &resp, &mut write_crypto_cursor)
            .await
            .inspect_err(
                |e| error!(?e, ?header.upstream, "Failed to write response to downstream"),
            )?;

        // Return upstream
        Ok((upstream, header.upstream, sock_addr))
    }

    #[instrument(skip(self, stream))]
    async fn respond_with_error<S>(
        &self,
        stream: &mut S,
        error: ProxyProtocolError,
    ) -> Result<(), ProxyProtocolError>
    where
        S: IoStream + IoAddr,
    {
        let local_addr = stream
            .local_addr()
            .inspect_err(|e| error!(?e, "Failed to get local address"))?;

        // Respond with error
        let resp = match error {
            ProxyProtocolError::Io(_) => ResponseHeader {
                result: Err(ResponseError {
                    source: local_addr.into(),
                    kind: ResponseErrorKind::Io,
                }),
            },
            ProxyProtocolError::Bincode(_) => ResponseHeader {
                result: Err(ResponseError {
                    source: local_addr.into(),
                    kind: ResponseErrorKind::Codec,
                }),
            },
            ProxyProtocolError::Loopback => ResponseHeader {
                result: Err(ResponseError {
                    source: local_addr.into(),
                    kind: ResponseErrorKind::Loopback,
                }),
            },
            ProxyProtocolError::Response(err) => ResponseHeader { result: Err(err) },
        };
        let mut crypto_cursor = XorCryptoCursor::new(&self.crypto);
        write_header_async(stream, &resp, &mut crypto_cursor)
            .await
            .inspect_err(|e| {
                let peer_addr = stream.peer_addr().ok();
                error!(
                    ?e,
                    ?peer_addr,
                    "Failed to write response to downstream after error"
                )
            })?;

        Ok(())
    }
}

#[derive(Debug)]
pub enum AcceptedStream {
    Quic { recv: RecvStream, send: SendStream },
    Tcp(TcpStream),
}

impl AsyncWrite for AcceptedStream {
    fn poll_write(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> std::task::Poll<Result<usize, io::Error>> {
        match self.deref_mut() {
            AcceptedStream::Quic { send, .. } => Pin::new(send).poll_write(cx, buf),
            AcceptedStream::Tcp(x) => Pin::new(x).poll_write(cx, buf),
        }
    }

    fn poll_flush(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), io::Error>> {
        match self.deref_mut() {
            AcceptedStream::Quic { send, .. } => Pin::new(send).poll_flush(cx),
            AcceptedStream::Tcp(x) => Pin::new(x).poll_flush(cx),
        }
    }

    fn poll_shutdown(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), io::Error>> {
        match self.deref_mut() {
            AcceptedStream::Quic { send, .. } => Pin::new(send).poll_shutdown(cx),
            AcceptedStream::Tcp(x) => Pin::new(x).poll_shutdown(cx),
        }
    }
}

impl AsyncRead for AcceptedStream {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<io::Result<()>> {
        match self.deref_mut() {
            AcceptedStream::Quic { recv, .. } => Pin::new(recv).poll_read(cx, buf),
            AcceptedStream::Tcp(x) => Pin::new(x).poll_read(cx, buf),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Deserialize)]
pub struct TcpProxyServerBuilder {
    pub listen_addr: String,
    pub header_xor_key: Vec<u8>,
    pub payload_xor_key: Option<Vec<u8>>,
}

impl TcpProxyServerBuilder {
    pub async fn build(self) -> io::Result<TcpServer<TcpProxyServer>> {
        let header_crypto = XorCrypto::new(self.header_xor_key);
        let payload_crypto = self.payload_xor_key.map(XorCrypto::new);
        let tcp_proxy = TcpProxyServer::new(header_crypto, payload_crypto);
        let server = tcp_proxy.build(self.listen_addr).await?;
        Ok(server)
    }
}

pub struct TcpProxyServer {
    acceptor: TcpProxyAcceptor,
    payload_crypto: Option<XorCrypto>,
}

impl TcpProxyServer {
    pub fn new(header_crypto: XorCrypto, payload_crypto: Option<XorCrypto>) -> Self {
        Self {
            acceptor: TcpProxyAcceptor::new(header_crypto, HashMap::new()),
            payload_crypto,
        }
    }

    pub async fn build(self, listen_addr: impl ToSocketAddrs) -> io::Result<TcpServer<Self>> {
        let listener = TcpListener::bind(listen_addr)
            .await
            .inspect_err(|e| error!(?e, "Failed to bind to listen address"))?;
        let server = TcpServer::new(listener, self);
        Ok(server)
    }

    #[instrument(skip(self, downstream))]
    async fn proxy<S>(&self, mut downstream: S) -> io::Result<()>
    where
        S: IoStream + IoAddr,
    {
        let start = std::time::Instant::now();

        let downstream_addr = downstream
            .peer_addr()
            .inspect_err(|e| error!(?e, "Failed to get downstream address"))?;

        // Establish proxy chain
        let (mut upstream, upstream_addr, upstream_sock_addr) =
            match self.acceptor.establish(&mut downstream).await {
                Ok(upstream) => upstream,
                Err(e) => {
                    self.handle_proxy_error(&mut downstream, e).await;
                    return Ok(());
                }
            };

        // Copy data
        let res = match &self.payload_crypto {
            Some(crypto) => {
                // Establish encrypted stream
                let mut xor_stream = XorStream::upgrade(downstream, crypto);
                tokio::io::copy_bidirectional(&mut xor_stream, &mut upstream).await
            }
            None => tokio::io::copy_bidirectional(&mut downstream, &mut upstream).await,
        };
        let (bytes_uplink, bytes_downlink) =
            res.inspect_err(|e| error!(?e, ?upstream, "Failed to copy data between streams"))?;

        let end = std::time::Instant::now();
        let metrics = StreamMetrics {
            start,
            end,
            bytes_uplink,
            bytes_downlink,
            upstream_addr,
            upstream_sock_addr,
            downstream_addr,
        };
        info!(%metrics, "Connection closed normally");
        Ok(())
    }

    #[instrument(skip(self, stream, e))]
    async fn handle_proxy_error<S>(&self, stream: &mut S, e: ProxyProtocolError)
    where
        S: IoStream + IoAddr,
    {
        error!(?e, "Connection closed with error");
        let _ = self
            .acceptor
            .respond_with_error(stream, e)
            .await
            .inspect_err(|e| {
                let peer_addr = stream.peer_addr().ok();
                error!(
                    ?e,
                    ?peer_addr,
                    "Failed to respond with error to downstream after error"
                )
            });
    }
}

#[async_trait]
impl StreamServerHook for TcpProxyServer {
    #[instrument(skip(self, stream))]
    async fn handle_stream<S>(&self, stream: S)
    where
        S: IoStream + IoAddr,
    {
        if let Err(e) = self.proxy(stream).await {
            error!(?e, "Connection closed with error");
        }
    }
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
            let proxy = TcpProxyServer::new(crypto.clone(), None);
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
                upstream: origin_addr.into(),
            };
            let mut crypto_cursor = XorCryptoCursor::new(&crypto);
            write_header_async(&mut stream, &header, &mut crypto_cursor)
                .await
                .unwrap();

            // Read response
            let mut crypto_cursor = XorCryptoCursor::new(&crypto);
            let resp: ResponseHeader = read_header_async(&mut stream, &mut crypto_cursor)
                .await
                .unwrap();
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
