use std::{
    io,
    net::SocketAddr,
    pin::Pin,
    sync::{Arc, RwLock},
};

use async_trait::async_trait;
use metrics::counter;
use serde::Deserialize;
use thiserror::Error;
use tokio::{
    io::{AsyncRead, AsyncWrite},
    net::{TcpListener, TcpSocket, TcpStream, ToSocketAddrs},
};
use tracing::{info, instrument, trace, warn};

use common::{
    addr::any_addr,
    connect::ConnectorConfig,
    error::AnyResult,
    loading,
    proto::{
        conn_handler::{
            ListenerBindError,
            stream::{
                StreamProxyConnHandler, StreamProxyConnHandlerBuilder,
                StreamProxyConnHandlerConfig, StreamProxyServerBuildError,
            },
        },
        connect::stream::StreamConnect,
        context::StreamContext,
    },
    stream::{AsConn, HasIoAddr, OwnIoStream, StreamServerHandleConn},
};

pub const TCP_STREAM_TYPE: &str = "tcp";

#[derive(Debug)]
pub struct TcpServer<ConnHandler> {
    listener: TcpListener,
    conn_handler: ConnHandler,
}
impl<ConnHandler> TcpServer<ConnHandler> {
    pub fn new(listener: TcpListener, conn_handler: ConnHandler) -> Self {
        Self {
            listener,
            conn_handler,
        }
    }

    pub fn listener(&self) -> &TcpListener {
        &self.listener
    }

    pub fn listener_mut(&mut self) -> &mut TcpListener {
        &mut self.listener
    }
}
impl<ConnHandler> loading::Serve for TcpServer<ConnHandler>
where
    ConnHandler: StreamServerHandleConn + Send + Sync + 'static,
{
    type ConnHandler = ConnHandler;

    async fn serve(
        self,
        set_conn_handler_rx: loading::ReplaceConnHandlerRx<Self::ConnHandler>,
    ) -> AnyResult {
        self.serve_(set_conn_handler_rx).await.map_err(|e| e.into())
    }
}
impl<ConnHandler> TcpServer<ConnHandler>
where
    ConnHandler: StreamServerHandleConn + Send + Sync + 'static,
{
    #[instrument(skip(self))]
    async fn serve_(
        self,
        mut set_conn_handler_rx: loading::ReplaceConnHandlerRx<ConnHandler>,
    ) -> Result<(), ServeError> {
        let addr = self.listener.local_addr().map_err(ServeError::LocalAddr)?;
        info!(?addr, "Listening");
        // Arc conn_handler
        let mut conn_handler = Arc::new(self.conn_handler);
        loop {
            trace!("Waiting for connection");
            tokio::select! {
                res = self.listener.accept() => {
                    // let (stream, _) = res.map_err(|e| ServeError::Accept { source: e, addr })?;
                    let (stream, _) = match res {
                        Ok(res) => res,
                        Err(e) => {
                            warn!(?e, ?addr, "Accept error");
                            continue;
                        }
                    };
                    counter!("stream.tcp.accepts").increment(1);
                    // Arc conn_handler
                    let conn_handler = Arc::clone(&conn_handler);
                    tokio::spawn(async move {
                        conn_handler.handle_stream(IoTcpStream(stream)).await;
                    });
                }
                res = set_conn_handler_rx.0.recv() => {
                    let new_conn_handler = match res {
                        Some(new_conn_handler) => new_conn_handler,
                        None => break,
                    };
                    info!(?addr, "Connection handler set");
                    conn_handler = Arc::new(new_conn_handler);
                }
            }
        }
        Ok(())
    }
}
#[derive(Debug, Error)]
pub enum ServeError {
    #[error("Failed to get local address: {0}")]
    LocalAddr(#[source] io::Error),
    #[error("Failed to accept connection: {source}, {addr}")]
    Accept {
        #[source]
        source: io::Error,
        addr: SocketAddr,
    },
}

#[derive(Debug, Clone)]
pub struct TcpConnector {
    config: Arc<RwLock<ConnectorConfig>>,
}
impl TcpConnector {
    pub fn new(config: Arc<RwLock<ConnectorConfig>>) -> Self {
        Self { config }
    }
}
#[async_trait]
impl StreamConnect for TcpConnector {
    async fn connect(&self, addr: SocketAddr) -> io::Result<Box<dyn AsConn>> {
        let bind = self
            .config
            .read()
            .unwrap()
            .bind
            .get_matched(&addr.ip())
            .map(|ip| SocketAddr::new(ip, 0))
            .unwrap_or_else(|| any_addr(&addr.ip()));
        let socket = match addr.ip() {
            std::net::IpAddr::V4(_) => TcpSocket::new_v4()?,
            std::net::IpAddr::V6(_) => TcpSocket::new_v6()?,
        };
        socket.bind(bind)?;
        let stream = socket.connect(addr).await?;
        counter!("stream.tcp.connects").increment(1);
        Ok(Box::new(IoTcpStream(stream)))
    }
}

#[derive(Debug)]
pub struct IoTcpStream(pub TcpStream);
impl AsyncWrite for IoTcpStream {
    fn poll_write(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> std::task::Poll<Result<usize, io::Error>> {
        std::pin::Pin::new(&mut self.0).poll_write(cx, buf)
    }

    fn poll_flush(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), io::Error>> {
        std::pin::Pin::new(&mut self.0).poll_flush(cx)
    }

    fn poll_shutdown(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), io::Error>> {
        std::pin::Pin::new(&mut self.0).poll_shutdown(cx)
    }
}
impl AsyncRead for IoTcpStream {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<io::Result<()>> {
        std::pin::Pin::new(&mut self.0).poll_read(cx, buf)
    }
}
impl AsConn for IoTcpStream {}
impl OwnIoStream for IoTcpStream {}
impl HasIoAddr for IoTcpStream {
    fn peer_addr(&self) -> io::Result<SocketAddr> {
        self.0.peer_addr()
    }
    fn local_addr(&self) -> io::Result<SocketAddr> {
        self.0.local_addr()
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TcpProxyServerConfig {
    pub listen_addr: Arc<str>,
    #[serde(flatten)]
    pub inner: StreamProxyConnHandlerConfig,
}
impl TcpProxyServerConfig {
    pub fn into_builder(self, stream_context: StreamContext) -> TcpProxyServerBuilder {
        let listen_addr = Arc::clone(&self.listen_addr);
        let inner = self.inner.into_builder(stream_context, listen_addr);
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

    use crate::stream::connect::build_concrete_stream_connector_table;

    use super::*;
    use ae::anti_replay::ReplayValidator;
    use common::{
        addr::BothVerIp,
        anti_replay::{VALIDATOR_CAPACITY, VALIDATOR_TIME_FRAME},
        connect::ConnectorConfig,
        header::{codec::write_header_async, heartbeat},
        loading::Serve,
        proto::{addr::StreamAddr, context::StreamContext, header::StreamRequestHeader},
        stream::pool::StreamConnPool,
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
            let listen_addr = Arc::from("localhost:0");
            let connector_config = ConnectorConfig {
                bind: BothVerIp { v4: None, v6: None },
            };
            let proxy = StreamProxyConnHandler::new(
                crypto.clone(),
                None,
                StreamContext {
                    session_table: None,
                    pool: Swap::new(StreamConnPool::empty()),
                    connector_table: Arc::new(build_concrete_stream_connector_table(
                        connector_config,
                    )),
                    replay_validator: Arc::new(ReplayValidator::new(
                        VALIDATOR_TIME_FRAME,
                        VALIDATOR_CAPACITY,
                    )),
                },
                Arc::clone(&listen_addr),
            );

            let server = build_tcp_proxy_server(listen_addr.as_ref(), proxy)
                .await
                .unwrap();
            let proxy_addr = server.listener().local_addr().unwrap();
            tokio::spawn(async move {
                let (_set_conn_handler_tx, set_conn_handler_rx) =
                    loading::replace_conn_handler_channel();
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
                    stream_type: TCP_STREAM_TYPE.into(),
                }),
            };
            write_header_async(&mut stream, &header, *crypto.key())
                .await
                .unwrap();
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
