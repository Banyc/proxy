use std::{io, sync::Arc};

use async_trait::async_trait;
use common::{loading, stream::streams::tcp::TcpServer};
use serde::Deserialize;
use thiserror::Error;
use tokio::net::{TcpListener, ToSocketAddrs};
use tracing::error;

use super::{StreamProxyServer, StreamProxyServerBuilder};

#[derive(Debug, Clone, PartialEq, Eq, Hash, Deserialize)]
pub struct TcpProxyServerBuilder {
    pub listen_addr: Arc<str>,
    #[serde(flatten)]
    pub inner: StreamProxyServerBuilder,
}

#[async_trait]
impl loading::Builder for TcpProxyServerBuilder {
    type Hook = StreamProxyServer;
    type Server = TcpServer<Self::Hook>;

    async fn build_server(self) -> io::Result<Self::Server> {
        let stream_proxy = self.inner.build();
        build_tcp_proxy_server(self.listen_addr.as_ref(), stream_proxy)
            .await
            .map_err(|e| e.0)
    }

    fn build_hook(self) -> io::Result<Self::Hook> {
        Ok(self.inner.build())
    }

    fn key(&self) -> &Arc<str> {
        &self.listen_addr
    }
}

pub async fn build_tcp_proxy_server(
    listen_addr: impl ToSocketAddrs,
    stream_proxy: StreamProxyServer,
) -> Result<TcpServer<StreamProxyServer>, ListenerBindError> {
    let listener = TcpListener::bind(listen_addr).await?;
    let server = TcpServer::new(listener, stream_proxy);
    Ok(server)
}

#[derive(Debug, Error)]
#[error("Failed to bind to listen address")]
pub struct ListenerBindError(#[from] io::Error);

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;
    use common::{
        crypto::{XorCrypto, XorCryptoCursor},
        header::{codec::write_header_async, heartbeat},
        loading::Server,
        stream::{
            addr::{StreamAddr, StreamType},
            header::StreamRequestHeader,
            pool::Pool,
        },
    };
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::{TcpListener, TcpStream},
    };

    #[tokio::test(flavor = "multi_thread")]
    async fn test_proxy() {
        let crypto = XorCrypto::new(vec![].into());

        // Start proxy server
        let proxy_addr = {
            let proxy = StreamProxyServer::new(crypto.clone(), None, Pool::new());

            let server = build_tcp_proxy_server("localhost:0", proxy).await.unwrap();
            let proxy_addr = server.listener().local_addr().unwrap();
            tokio::spawn(async move {
                let _handle = server.handle().clone();
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
            heartbeat::send_upgrade(&mut stream, Duration::from_secs(1))
                .await
                .unwrap();
            // Encode header
            let header = StreamRequestHeader {
                upstream: StreamAddr {
                    address: origin_addr.into(),
                    stream_type: StreamType::Tcp,
                },
            };
            let mut crypto_cursor = XorCryptoCursor::new(&crypto);
            write_header_async(&mut stream, &header, &mut crypto_cursor)
                .await
                .unwrap();

            // // Read response
            // let mut crypto_cursor = XorCryptoCursor::new(&crypto);
            // let resp: ResponseHeader = read_header_async(&mut stream, &mut crypto_cursor)
            //     .await
            //     .unwrap();
            // assert!(resp.result.is_ok());
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
