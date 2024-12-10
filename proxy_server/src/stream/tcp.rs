use std::sync::Arc;

use common::loading;
use protocol::stream::{context::ConcreteStreamContext, streams::tcp::TcpServer};
use serde::Deserialize;
use thiserror::Error;
use tokio::net::{TcpListener, ToSocketAddrs};
use tracing::error;

use crate::ListenerBindError;

use super::{
    StreamProxyConnHandler, StreamProxyConnHandlerBuilder, StreamProxyServerBuildError,
    StreamProxyServerConfig,
};

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TcpProxyServerConfig {
    pub listen_addr: Arc<str>,
    #[serde(flatten)]
    pub inner: StreamProxyServerConfig,
}
impl TcpProxyServerConfig {
    pub fn into_builder(self, stream_context: ConcreteStreamContext) -> TcpProxyServerBuilder {
        let inner = self.inner.into_builder(stream_context);
        TcpProxyServerBuilder {
            listen_addr: self.listen_addr,
            inner,
        }
    }
}

#[derive(Debug, Clone)]
pub struct TcpProxyServerBuilder {
    pub listen_addr: Arc<str>,
    pub inner: StreamProxyConnHandlerBuilder,
}
impl loading::Build for TcpProxyServerBuilder {
    type ConnHandler = StreamProxyConnHandler;
    type Server = TcpServer<Self::ConnHandler>;
    type Err = TcpProxyServerBuildError;

    async fn build_server(self) -> Result<Self::Server, Self::Err> {
        let listen_addr = self.listen_addr.clone();
        let stream_proxy = self.build_conn_handler()?;
        build_tcp_proxy_server(listen_addr.as_ref(), stream_proxy)
            .await
            .map_err(|e| e.into())
    }

    fn build_conn_handler(self) -> Result<Self::ConnHandler, Self::Err> {
        self.inner.build().map_err(|e| e.into())
    }

    fn key(&self) -> &Arc<str> {
        &self.listen_addr
    }
}
#[derive(Debug, Error)]
pub enum TcpProxyServerBuildError {
    #[error("{0}")]
    Hook(#[from] StreamProxyServerBuildError),
    #[error("{0}")]
    Server(#[from] ListenerBindError),
}
pub async fn build_tcp_proxy_server(
    listen_addr: impl ToSocketAddrs,
    stream_proxy: StreamProxyConnHandler,
) -> Result<TcpServer<StreamProxyConnHandler>, ListenerBindError> {
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
        loading::Serve,
        stream::{addr::StreamAddr, header::StreamRequestHeader},
    };
    use protocol::stream::{
        addr::ConcreteStreamType, connect::ConcreteStreamConnectorTable, pool::ConcreteConnPool,
    };
    use swap::Swap;
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::TcpStream,
    };

    #[tokio::test(flavor = "multi_thread")]
    async fn test_proxy() {
        let crypto = tokio_chacha20::config::Config::new(vec![].into());

        // Start proxy server
        let proxy_addr = {
            let proxy = StreamProxyConnHandler::new(
                crypto.clone(),
                None,
                ConcreteStreamContext {
                    session_table: None,
                    pool: Swap::new(ConcreteConnPool::empty()),
                    connector_table: ConcreteStreamConnectorTable::new(),
                },
            );

            let server = build_tcp_proxy_server("localhost:0", proxy).await.unwrap();
            let proxy_addr = server.listener().local_addr().unwrap();
            tokio::spawn(async move {
                let (_set_conn_handler_tx, set_conn_handler_rx) = tokio::sync::mpsc::channel(64);
                server.serve(set_conn_handler_rx).await.unwrap();
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
            heartbeat::send_upgrade(&mut stream, Duration::from_secs(1), &crypto)
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
