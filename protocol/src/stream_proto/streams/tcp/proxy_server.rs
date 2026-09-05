use std::{io, net::SocketAddr, pin::Pin, sync::Arc};

use async_trait::async_trait;
use metrics::counter;
use serde::Deserialize;
use thiserror::Error;
use tokio::{
    io::{AsyncRead, AsyncWrite},
    net::{TcpListener, TcpSocket, TcpStream, ToSocketAddrs},
};

use common::{
    addr::any_addr,
    connect::ConnectorConfigReader,
    loading,
    proxy_runtime::{
        conn_handler::{
            ListenerBindError,
            stream::{
                StreamProxyConnHandler, StreamProxyConnHandlerBuilder,
                StreamProxyConnHandlerConfig, StreamProxyServerBuildError,
            },
        },
        connect::stream::StreamConnect,
        context::StreamRuntime,
    },
    session::SessionSpawner,
    stream_runtime::{HasIoAddr, IoConnection, OwnedIoStream},
};

use super::listener::TcpServer;

#[derive(Debug, Clone)]
pub struct TcpConnector {
    config: ConnectorConfigReader,
}
impl TcpConnector {
    pub fn new(config: ConnectorConfigReader) -> Self {
        Self { config }
    }
}
#[async_trait]
impl StreamConnect for TcpConnector {
    async fn connect(&self, addr: SocketAddr) -> io::Result<Box<dyn IoConnection>> {
        let bind = self
            .config
            .current()
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
        Ok(Box::new(AddressedTcpStream(stream)))
    }
}

#[derive(Debug)]
pub struct AddressedTcpStream(pub TcpStream);
impl AsyncWrite for AddressedTcpStream {
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
impl AsyncRead for AddressedTcpStream {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<io::Result<()>> {
        std::pin::Pin::new(&mut self.0).poll_read(cx, buf)
    }
}
impl IoConnection for AddressedTcpStream {}
impl OwnedIoStream for AddressedTcpStream {}
impl HasIoAddr for AddressedTcpStream {
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
    pub fn into_builder(self, stream_context: StreamRuntime) -> TcpProxyServerBuilder {
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
        let session_spawner = self.inner.stream_context.session_spawner.clone();
        let stream_proxy = self.build_conn_handler()?;
        build_tcp_proxy_server(listen_addr.as_ref(), stream_proxy, session_spawner)
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
    session_spawner: SessionSpawner,
) -> Result<TcpServer<StreamProxyConnHandler>, ListenerBindError> {
    let listener = TcpListener::bind(listen_addr)
        .await
        .map_err(ListenerBindError)?;
    let server = TcpServer::new(listener, stream_proxy, session_spawner);
    Ok(server)
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use crate::stream_proto::connect::build_concrete_stream_connector_table;

    use super::*;
    use crate::stream_proto::streams::tcp::listener::TCP_STREAM_TYPE;
    use ae::anti_replay::ReplayValidator;
    use common::{
        addr::DualStackBind,
        anti_replay::{VALIDATOR_CAPACITY, VALIDATOR_TIME_FRAME},
        connect::{ConnectorConfig, ConnectorResetSignal, connector_config_cell},
        header::{codec::write_header_async, preamble},
        loading::Serve,
        notify::Notify,
        proxy_runtime::{
            addr::RouteAddr, connect::udp::UdpConnector, context::StreamRuntime,
            header::StreamRequestHeader,
        },
        stream_runtime::pool::StreamConnPool,
    };
    use swap::Swap;
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::TcpStream,
    };

    #[tokio::test(flavor = "multi_thread")]
    async fn test_proxy() {
        let crypto = tokio_chacha20::config::Config::new(vec![].into());
        let connector_reset = ConnectorResetSignal(Notify::new());
        let (session_spawner, mut session_rx) = common::session::SessionSpawner::channel();
        let (retention_actor, retention) = common::lifecycle::retention::RetentionActor::new();
        // Test-owned tasks; aborted via JoinSet drop at the end of the test.
        let mut test_tasks: tokio::task::JoinSet<()> = tokio::task::JoinSet::new();
        test_tasks.spawn(async move {
            let _retention_actor = retention_actor;
            let mut sessions = tokio::task::JoinSet::new();
            loop {
                tokio::select! {
                    Some(fut) = session_rx.recv() => { sessions.spawn(fut); }
                    Some(res) = sessions.join_next() => { let _ = res.unwrap(); }
                    else => break,
                }
            }
        });
        let proxy_addr = {
            let listen_addr = Arc::from("localhost:0");
            let (connector_config, _updater) = connector_config_cell(ConnectorConfig {
                bind: DualStackBind { v4: None, v6: None },
                obfuscation_key: None,
            });
            let mut connector_drivers = tokio::task::JoinSet::new();
            let udp_connector = UdpConnector::new(connector_config.clone());
            let connector_table = Arc::new(build_concrete_stream_connector_table(
                connector_config,
                connector_reset,
                &mut connector_drivers,
                &udp_connector,
            ));
            test_tasks.spawn(async move {
                while let Some(result) = connector_drivers.join_next().await {
                    let _ = result.unwrap();
                }
            });
            let proxy = StreamProxyConnHandler::new(
                crypto.clone(),
                None,
                StreamRuntime {
                    session_table: None,
                    pool: Swap::new(StreamConnPool::empty()),
                    connector_table,
                    replay_validator: Arc::new(ReplayValidator::new(
                        VALIDATOR_TIME_FRAME,
                        VALIDATOR_CAPACITY,
                    )),
                    session_spawner: session_spawner.clone(),
                    retention: retention.clone(),
                },
                Arc::clone(&listen_addr),
                true,
                common::proxy_runtime::conn_handler::SpeedLimit::UNLIMITED,
            );
            let server =
                build_tcp_proxy_server(listen_addr.as_ref(), proxy, session_spawner.clone())
                    .await
                    .unwrap();
            let proxy_addr = server.listener().local_addr().unwrap();
            // Test-owned server task; lifetime is the test runtime.
            test_tasks.spawn(async move {
                let (_set_conn_handler_tx, set_conn_handler_rx) =
                    loading::replace_conn_handler_channel();
                server.serve(set_conn_handler_rx).await.unwrap();
            });
            proxy_addr
        };
        let req_msg = b"hello world";
        let resp_msg = b"goodbye world";
        let origin_addr = {
            let listener = TcpListener::bind("[::]:0").await.unwrap();
            let origin_addr = listener.local_addr().unwrap();
            // Test-owned origin server; lifetime is the test runtime.
            test_tasks.spawn(async move {
                let (mut stream, _) = listener.accept().await.unwrap();
                let mut buf = [0; 1024];
                let msg_buf = &mut buf[..req_msg.len()];
                stream.read_exact(msg_buf).await.unwrap();
                assert_eq!(msg_buf, req_msg);
                stream.write_all(resp_msg).await.unwrap();
            });
            origin_addr
        };
        let mut stream = TcpStream::connect(proxy_addr).await.unwrap();
        {
            preamble::send_upgrade(&mut stream, Duration::from_secs(1), &crypto)
                .await
                .unwrap();
            let header = StreamRequestHeader {
                upstream: Some(RouteAddr {
                    address: origin_addr.into(),
                    protocol: TCP_STREAM_TYPE.into(),
                }),
            };
            write_header_async(&mut stream, &header, *crypto.key())
                .await
                .unwrap();
        }
        stream.write_all(req_msg).await.unwrap();
        {
            let mut buf = [0; 1024];
            let msg_buf = &mut buf[..resp_msg.len()];
            stream.read_exact(msg_buf).await.unwrap();
            assert_eq!(msg_buf, resp_msg);
        }
        // The reaper only exits once every `SessionSpawner` sender is dropped;
        // the proxy server keeps one alive for the test's lifetime, so the
        // reaper cannot finish on its own. Dropping `test_tasks` at the end of
        // the test aborts the server/origin/reaper as the backstop.
    }
}
