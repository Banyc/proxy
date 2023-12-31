use std::sync::Arc;

use common::{
    loading,
    stream::concrete::{pool::Pool, streams::tcp::TcpServer},
};
use serde::Deserialize;
use thiserror::Error;
use tokio::net::{TcpListener, ToSocketAddrs};
use tracing::error;

use crate::ListenerBindError;

use super::{StreamProxy, StreamProxyBuildError, StreamProxyBuilder, StreamProxyConfig};

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TcpProxyServerConfig {
    pub listen_addr: Arc<str>,
    #[serde(flatten)]
    pub inner: StreamProxyConfig,
}

impl TcpProxyServerConfig {
    pub fn into_builder(self, stream_pool: Pool) -> TcpProxyServerBuilder {
        let inner = self.inner.into_builder(stream_pool);
        TcpProxyServerBuilder {
            listen_addr: self.listen_addr,
            inner,
        }
    }
}

#[derive(Debug, Clone)]
pub struct TcpProxyServerBuilder {
    pub listen_addr: Arc<str>,
    pub inner: StreamProxyBuilder,
}

impl loading::Builder for TcpProxyServerBuilder {
    type Hook = StreamProxy;
    type Server = TcpServer<Self::Hook>;
    type Err = TcpProxyServerBuildError;

    async fn build_server(self) -> Result<Self::Server, Self::Err> {
        let listen_addr = self.listen_addr.clone();
        let stream_proxy = self.build_hook()?;
        build_tcp_proxy_server(listen_addr.as_ref(), stream_proxy)
            .await
            .map_err(|e| e.into())
    }

    fn build_hook(self) -> Result<Self::Hook, Self::Err> {
        self.inner.build().map_err(|e| e.into())
    }

    fn key(&self) -> &Arc<str> {
        &self.listen_addr
    }
}

#[derive(Debug, Error)]
pub enum TcpProxyServerBuildError {
    #[error("{0}")]
    Hook(#[from] StreamProxyBuildError),
    #[error("{0}")]
    Server(#[from] ListenerBindError),
}

pub async fn build_tcp_proxy_server(
    listen_addr: impl ToSocketAddrs,
    stream_proxy: StreamProxy,
) -> Result<TcpServer<StreamProxy>, ListenerBindError> {
    let listener = TcpListener::bind(listen_addr)
        .await
        .map_err(ListenerBindError)?;
    let server = TcpServer::new(listener, stream_proxy);
    Ok(server)
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;
    use common::{
        header::{codec::write_header_async, heartbeat},
        loading::Server,
        stream::{
            addr::StreamAddr, concrete::addr::ConcreteStreamType, header::StreamRequestHeader,
        },
    };
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::{TcpListener, TcpStream},
    };

    #[tokio::test(flavor = "multi_thread")]
    async fn test_proxy() {
        let crypto = tokio_chacha20::config::Config::new(vec![].into());

        // Start proxy server
        let proxy_addr = {
            let proxy = StreamProxy::new(crypto.clone(), None, Pool::empty());

            let server = build_tcp_proxy_server("localhost:0", proxy).await.unwrap();
            let proxy_addr = server.listener().local_addr().unwrap();
            tokio::spawn(async move {
                let _handle = server.handle();
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
                upstream: Some(StreamAddr {
                    address: origin_addr.into(),
                    stream_type: ConcreteStreamType::Tcp,
                }),
            };
            let mut crypto_cursor = tokio_chacha20::cursor::EncryptCursor::new(*crypto.key());
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
