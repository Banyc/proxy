use std::{io, net::SocketAddr, pin::Pin, sync::Arc};

use async_trait::async_trait;
use metrics::counter;
use serde::Deserialize;
use thiserror::Error;
use tokio::{
    io::{AsyncRead, AsyncWrite},
    net::{ToSocketAddrs, UdpSocket},
};
use tokio_kcp::{KcpConfig, KcpListener, KcpNoDelayConfig, KcpStream};
use tracing::instrument;

use common::{
    addr::any_addr,
    connect::ConnectorConfigReader,
    error::AnyResult,
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
    session::{SessionSpawner, log_rejection},
    stream_runtime::{HasIoAddr, IoConnection, OwnedIoStream, StreamServerHandleConn},
};

#[derive(Debug)]
pub struct KcpServer<ConnHandler> {
    listener: KcpListener,
    conn_handler: ConnHandler,
    session_spawner: SessionSpawner,
}
impl<ConnHandler> KcpServer<ConnHandler> {
    pub fn new(
        listener: KcpListener,
        conn_handler: ConnHandler,
        session_spawner: SessionSpawner,
    ) -> Self {
        Self {
            listener,
            conn_handler,
            session_spawner,
        }
    }

    pub fn listener(&self) -> &KcpListener {
        &self.listener
    }

    pub fn listener_mut(&mut self) -> &mut KcpListener {
        &mut self.listener
    }
}
impl<ConnHandler> loading::Serve for KcpServer<ConnHandler>
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
impl<ConnHandler> KcpServer<ConnHandler>
where
    ConnHandler: StreamServerHandleConn + Send + Sync + 'static,
{
    #[instrument(skip_all)]
    async fn serve_(
        self,
        set_conn_handler_rx: loading::ReplaceConnHandlerRx<ConnHandler>,
    ) -> Result<(), ServeLoopError> {
        let addr = self
            .listener
            .local_addr()
            .map_err(ServeLoopError::LocalAddr)?;
        let listener = Arc::new(tokio::sync::Mutex::new(self.listener));
        let accept_listener = Arc::clone(&listener);
        let session_spawner = self.session_spawner.clone();
        let mut state = ();
        common::lifecycle::serve_loop::serve_loop(
            addr,
            Arc::new(self.conn_handler),
            set_conn_handler_rx,
            |_| {},
            || {
                let accept_listener = Arc::clone(&accept_listener);
                async move {
                    accept_listener
                        .lock()
                        .await
                        .accept()
                        .await
                        .map_err(Into::into)
                }
            },
            |_, (stream, peer_addr): (KcpStream, SocketAddr), conn_handler: Arc<ConnHandler>| {
                let stream = AddressedKcpStream {
                    stream,
                    local_addr: addr,
                    peer_addr,
                };
                let session_spawner = session_spawner.clone();
                Box::pin(async move {
                    if let Err(error) = session_spawner
                        .spawn(async move {
                            conn_handler.handle_stream(stream).await;
                            Ok(())
                        })
                        .await
                    {
                        log_rejection("kcp", error);
                    }
                })
            },
            &mut state,
            |_| Box::pin(std::future::pending::<()>()),
            common::lifecycle::serve_loop::ServeLoopConfig {
                label: "kcp",
                counter_name: Some("stream.kcp.accepts"),
                counts_dispatch_errors: false,
            },
        )
        .await
    }
}
pub use common::lifecycle::serve_loop::ServeLoopError;

#[derive(Debug, Clone)]
pub struct KcpConnector {
    config: ConnectorConfigReader,
}
impl KcpConnector {
    pub fn new(config: ConnectorConfigReader) -> Self {
        Self { config }
    }
}
#[async_trait]
impl StreamConnect for KcpConnector {
    async fn connect(
        &self,
        addr: SocketAddr,
        _obfuscation_key: Option<[u8; 32]>,
    ) -> io::Result<Box<dyn IoConnection>> {
        let bind = self
            .config
            .current()
            .bind
            .get_matched(&addr.ip())
            .map(|ip| SocketAddr::new(ip, 0))
            .unwrap_or_else(|| any_addr(&addr.ip()));
        let socket = UdpSocket::bind(bind).await?;
        let local_addr = socket.local_addr()?;
        let config = fast_kcp_config();
        let stream = KcpStream::connect_with_socket(&config, socket, addr).await?;
        let stream = AddressedKcpStream {
            stream,
            local_addr,
            peer_addr: addr,
        };
        counter!("stream.kcp.connects").increment(1);
        Ok(Box::new(stream))
    }
}

pub fn fast_kcp_config() -> KcpConfig {
    KcpConfig {
        /* cSpell:disable */
        nodelay: KcpNoDelayConfig::fastest(),
        ..Default::default()
    }
}

#[derive(Debug)]
pub struct AddressedKcpStream {
    stream: KcpStream,
    local_addr: SocketAddr,
    peer_addr: SocketAddr,
}
impl IoConnection for AddressedKcpStream {}
impl OwnedIoStream for AddressedKcpStream {}
impl HasIoAddr for AddressedKcpStream {
    fn peer_addr(&self) -> io::Result<SocketAddr> {
        Ok(self.peer_addr)
    }
    fn local_addr(&self) -> io::Result<SocketAddr> {
        Ok(self.local_addr)
    }
}
impl AsyncRead for AddressedKcpStream {
    fn poll_read(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<io::Result<()>> {
        Pin::new(&mut self.stream).poll_read(cx, buf)
    }
}
impl AsyncWrite for AddressedKcpStream {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> std::task::Poll<Result<usize, io::Error>> {
        Pin::new(&mut self.stream).poll_write(cx, buf)
    }

    fn poll_flush(
        mut self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), io::Error>> {
        Pin::new(&mut self.stream).poll_flush(cx)
    }

    fn poll_shutdown(
        mut self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), io::Error>> {
        Pin::new(&mut self.stream).poll_shutdown(cx)
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct KcpProxyServerConfig {
    pub listen_addr: Arc<str>,
    #[serde(flatten)]
    pub inner: StreamProxyConnHandlerConfig,
}
impl KcpProxyServerConfig {
    pub fn into_builder(self, stream_context: StreamRuntime) -> KcpProxyServerBuilder {
        let listen_addr = Arc::clone(&self.listen_addr);
        let inner = self.inner.into_builder(stream_context, listen_addr);
        KcpProxyServerBuilder {
            listen_addr: self.listen_addr,
            inner,
        }
    }
}

#[derive(Debug, Clone)]
pub struct KcpProxyServerBuilder {
    pub listen_addr: Arc<str>,
    pub inner: StreamProxyConnHandlerBuilder,
}
impl loading::Build for KcpProxyServerBuilder {
    type ConnHandler = StreamProxyConnHandler;
    type Server = KcpServer<Self::ConnHandler>;
    type Err = KcpProxyServerBuildError;

    async fn build_server(self) -> Result<Self::Server, Self::Err> {
        let listen_addr = self.listen_addr.clone();
        let session_spawner = self.inner.stream_context.session_spawner.clone();
        let stream_proxy = self.build_conn_handler()?;
        build_kcp_proxy_server(listen_addr.as_ref(), stream_proxy, session_spawner)
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
pub enum KcpProxyServerBuildError {
    #[error("{0}")]
    Hook(#[from] StreamProxyServerBuildError),
    #[error("{0}")]
    Server(#[from] ListenerBindError),
}
pub async fn build_kcp_proxy_server(
    listen_addr: impl ToSocketAddrs,
    stream_proxy: StreamProxyConnHandler,
    session_spawner: SessionSpawner,
) -> Result<KcpServer<StreamProxyConnHandler>, ListenerBindError> {
    let config = fast_kcp_config();
    let listener = KcpListener::bind(config, listen_addr)
        .await
        .map_err(|e| ListenerBindError(e.into()))?;
    let server = KcpServer::new(listener, stream_proxy, session_spawner);
    Ok(server)
}
#[cfg(test)]
mod tests {
    use super::*;
    use common::addr::DualStackBind;
    use common::connect::ConnectorConfig;

    #[tokio::test]
    async fn a_connected_stream_reports_the_address_the_kernel_assigned() {
        let listener = KcpListener::bind(fast_kcp_config(), "127.0.0.1:0")
            .await
            .unwrap();
        let listen_addr = listener.local_addr().unwrap();
        let mut accept_tasks = tokio::task::JoinSet::new();
        accept_tasks.spawn(async move {
            let mut listener = listener;
            let _ = listener.accept().await;
        });
        let connector = KcpConnector::new(
            common::connect::connector_config_cell(ConnectorConfig {
                bind: DualStackBind { v4: None, v6: None },
            })
            .0,
        );
        let stream = connector.connect(listen_addr, None).await.unwrap();
        let local_addr = stream.local_addr().unwrap();
        assert_ne!(local_addr.port(), 0, "{local_addr}");
        drop(accept_tasks);
    }
}
