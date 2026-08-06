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
    net::ToSocketAddrs,
};
use tracing::instrument;

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
        context::StreamRuntime,
    },
    session::SessionSpawner,
    stream::{ConnParts, HasIoAddr, OwnIoStream, StreamServerHandleConn},
};

#[derive(Debug)]
pub struct RtpServer<ConnHandler> {
    listener: rtp::udp::Listener,
    conn_handler: ConnHandler,
    fec: bool,
    session_spawner: SessionSpawner,
}
impl<ConnHandler> RtpServer<ConnHandler> {
    pub fn new(
        listener: rtp::udp::Listener,
        conn_handler: ConnHandler,
        fec: bool,
        session_spawner: SessionSpawner,
    ) -> Self {
        Self {
            listener,
            conn_handler,
            fec,
            session_spawner,
        }
    }

    pub fn listener(&self) -> &rtp::udp::Listener {
        &self.listener
    }

    pub fn listener_mut(&mut self) -> &mut rtp::udp::Listener {
        &mut self.listener
    }
}
impl<ConnHandler> loading::Serve for RtpServer<ConnHandler>
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
impl<ConnHandler> RtpServer<ConnHandler>
where
    ConnHandler: StreamServerHandleConn + Send + Sync + 'static,
{
    #[instrument(skip_all)]
    async fn serve_(
        self,
        set_conn_handler_rx: loading::ReplaceConnHandlerRx<ConnHandler>,
    ) -> Result<(), ServeLoopError> {
        let addr = self.listener.local_addr();
        let listener = &self.listener;
        let fec = self.fec;
        let session_spawner = self.session_spawner.clone();
        let mut state = ();
        common::serve_loop::serve_loop(
            "rtp",
            Some("stream.rtp.accepts"),
            true,
            addr,
            Arc::new(self.conn_handler),
            set_conn_handler_rx,
            |_| {},
            || {
                listener.accept_without_handshake_with(rtp::udp::AcceptConfig {
                    fec,
                    ..rtp::udp::AcceptConfig::default()
                })
            },
            |_, stream: rtp::udp::Accepted, conn_handler: Arc<ConnHandler>| {
                let stream = AddressedRtpStream {
                    read: stream.read.into_async_read(),
                    write: stream.write.into_async_write(),
                    local_addr: listener.local_addr(),
                    peer_addr: stream.peer_addr,
                    _supervisor: stream.supervisor,
                };
                let session_spawner = session_spawner.clone();
                Box::pin(async move {
                    session_spawner
                        .spawn(async move {
                            conn_handler.handle_stream(stream).await;
                            Ok(())
                        })
                        .await;
                })
            },
            &mut state,
            |_| Box::pin(std::future::pending::<()>()),
        )
        .await
    }
}
pub use common::serve_loop::ServeLoopError;

#[derive(Debug, Clone)]
pub struct RtpConnector {
    config: Arc<RwLock<ConnectorConfig>>,
    fec: bool,
}
impl RtpConnector {
    pub fn new(config: Arc<RwLock<ConnectorConfig>>, fec: bool) -> Self {
        Self { config, fec }
    }
}
#[async_trait]
impl StreamConnect for RtpConnector {
    async fn connect(&self, addr: SocketAddr) -> io::Result<Box<dyn ConnParts>> {
        let bind = self
            .config
            .read()
            .unwrap()
            .bind
            .get_matched(&addr.ip())
            .map(|ip| SocketAddr::new(ip, 0))
            .unwrap_or_else(|| any_addr(&addr.ip()));
        let connected = rtp::udp::connect_with(
            bind,
            addr,
            rtp::udp::ConnectConfig {
                handshake: false,
                fec: self.fec,
                ..rtp::udp::ConnectConfig::default()
            },
        )
        .await?;
        let stream = AddressedRtpStream {
            read: connected.read.into_async_read(),
            write: connected.write.into_async_write(),
            local_addr: connected.local_addr,
            peer_addr: connected.peer_addr,
            _supervisor: connected.supervisor,
        };
        counter!("stream.rtp.connects").increment(1);
        Ok(Box::new(stream))
    }
}

#[derive(Debug)]
pub struct AddressedRtpStream {
    read: rtp::socket::AsyncReadAdapter,
    write: rtp::socket::AsyncWriteAdapter,
    local_addr: SocketAddr,
    peer_addr: SocketAddr,
    // Held for the stream's lifetime: dropping the RTP session handle aborts
    // the session driver tasks, so the reliable transport stalls without it.
    _supervisor: rtp::socket::SessionHandle,
}
impl ConnParts for AddressedRtpStream {}
impl OwnIoStream for AddressedRtpStream {}
impl HasIoAddr for AddressedRtpStream {
    fn peer_addr(&self) -> io::Result<SocketAddr> {
        Ok(self.peer_addr)
    }
    fn local_addr(&self) -> io::Result<SocketAddr> {
        Ok(self.local_addr)
    }
}
impl AsyncRead for AddressedRtpStream {
    fn poll_read(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<io::Result<()>> {
        Pin::new(&mut self.read).poll_read(cx, buf)
    }
}
impl AsyncWrite for AddressedRtpStream {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> std::task::Poll<Result<usize, io::Error>> {
        Pin::new(&mut self.write).poll_write(cx, buf)
    }

    fn poll_flush(
        mut self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), io::Error>> {
        Pin::new(&mut self.write).poll_flush(cx)
    }

    fn poll_shutdown(
        mut self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), io::Error>> {
        Pin::new(&mut self.write).poll_shutdown(cx)
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RtpProxyServerConfig {
    pub listen_addr: Arc<str>,
    #[serde(flatten)]
    pub inner: StreamProxyConnHandlerConfig,
}
impl RtpProxyServerConfig {
    pub fn into_builder(self, stream_context: StreamRuntime) -> RtpProxyServerBuilder {
        let listen_addr = Arc::clone(&self.listen_addr);
        let inner = self.inner.into_builder(stream_context, listen_addr);
        RtpProxyServerBuilder {
            listen_addr: self.listen_addr,
            inner,
        }
    }
}

#[derive(Debug, Clone)]
pub struct RtpProxyServerBuilder {
    pub listen_addr: Arc<str>,
    pub inner: StreamProxyConnHandlerBuilder,
}
impl loading::Build for RtpProxyServerBuilder {
    type ConnHandler = StreamProxyConnHandler;
    type Server = RtpServer<Self::ConnHandler>;
    type Err = RtpProxyServerBuildError;

    async fn build_server(self) -> Result<Self::Server, Self::Err> {
        let listen_addr = self.listen_addr.clone();
        let session_spawner = self.inner.stream_context.session_spawner.clone();
        let stream_proxy = self.build_conn_handler()?;
        build_rtp_proxy_server(listen_addr.as_ref(), stream_proxy, session_spawner)
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
pub enum RtpProxyServerBuildError {
    #[error("{0}")]
    Hook(#[from] StreamProxyServerBuildError),
    #[error("{0}")]
    Server(#[from] ListenerBindError),
}
pub async fn build_rtp_proxy_server(
    listen_addr: impl ToSocketAddrs,
    stream_proxy: StreamProxyConnHandler,
    session_spawner: SessionSpawner,
) -> Result<RtpServer<StreamProxyConnHandler>, ListenerBindError> {
    let fec = false;
    let listener = rtp::udp::Listener::bind(listen_addr)
        .await
        .map_err(ListenerBindError)?;
    let server = RtpServer::new(listener, stream_proxy, fec, session_spawner);
    Ok(server)
}
