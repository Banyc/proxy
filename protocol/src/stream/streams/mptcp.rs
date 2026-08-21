use std::{io, net::SocketAddr, num::NonZeroUsize, sync::Arc};

use async_trait::async_trait;
use metrics::counter;
use mptcp::{listen::MptcpListener, stream::MptcpStream};
use serde::Deserialize;
use thiserror::Error;
use tokio::{
    io::{AsyncRead, AsyncWrite},
    net::ToSocketAddrs,
};
use tracing::instrument;

use common::{
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
    session::{SessionSpawner, log_rejection},
    stream::{HasIoAddr, IoConnection, OwnedIoStream, StreamServerHandleConn},
};

const STREAMS: usize = 4;

#[derive(Debug)]
pub struct MptcpServer<ConnHandler> {
    listener: MptcpListener,
    conn_handler: ConnHandler,
    session_spawner: SessionSpawner,
}
impl<ConnHandler> MptcpServer<ConnHandler> {
    pub fn new(
        listener: MptcpListener,
        conn_handler: ConnHandler,
        session_spawner: SessionSpawner,
    ) -> Self {
        Self {
            listener,
            conn_handler,
            session_spawner,
        }
    }

    pub fn listener(&self) -> &MptcpListener {
        &self.listener
    }

    pub fn listener_mut(&mut self) -> &mut MptcpListener {
        &mut self.listener
    }
}
impl<ConnHandler> loading::Serve for MptcpServer<ConnHandler>
where
    ConnHandler: StreamServerHandleConn + Send + Sync + 'static,
{
    type ConnHandler = ConnHandler;

    async fn serve(
        self,
        set_conn_handler_rx: loading::ReplaceConnHandlerRx<ConnHandler>,
    ) -> AnyResult {
        self.serve_(set_conn_handler_rx).await.map_err(|e| e.into())
    }
}
impl<ConnHandler> MptcpServer<ConnHandler>
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
            .local_addrs()
            .next()
            .unwrap()
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
                async move { accept_listener.lock().await.accept().await }
            },
            |_, stream: MptcpStream, conn_handler: Arc<ConnHandler>| {
                let session_spawner = session_spawner.clone();
                Box::pin(async move {
                    if let Err(error) = session_spawner
                        .spawn(async move {
                            conn_handler
                                .handle_stream(AddressedMptcpStream(stream))
                                .await;
                            Ok(())
                        })
                        .await
                    {
                        log_rejection("mptcp", error);
                    }
                })
            },
            &mut state,
            |_| Box::pin(std::future::pending::<()>()),
            common::lifecycle::serve_loop::ServeLoopConfig {
                label: "mptcp",
                counter_name: Some("stream.mptcp.accepts"),
                counts_dispatch_errors: false,
            },
        )
        .await
    }
}
pub use common::lifecycle::serve_loop::ServeLoopError;

#[derive(Debug, Clone, Copy)]
pub struct MptcpConnector;
#[async_trait]
impl StreamConnect for MptcpConnector {
    async fn connect(&self, addr: SocketAddr) -> io::Result<Box<dyn IoConnection>> {
        let addrs = std::iter::repeat_n((), STREAMS).map(|()| addr);
        let stream = MptcpStream::connect(addrs).await?;
        counter!("stream.mptcp.connects").increment(1);
        Ok(Box::new(AddressedMptcpStream(stream)))
    }
}

#[derive(Debug)]
pub struct AddressedMptcpStream(pub MptcpStream);
impl AsyncWrite for AddressedMptcpStream {
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
impl AsyncRead for AddressedMptcpStream {
    fn poll_read(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<io::Result<()>> {
        std::pin::Pin::new(&mut self.0).poll_read(cx, buf)
    }
}
impl IoConnection for AddressedMptcpStream {}
impl OwnedIoStream for AddressedMptcpStream {}
impl HasIoAddr for AddressedMptcpStream {
    fn peer_addr(&self) -> io::Result<SocketAddr> {
        self.0.peer_addr().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::Unsupported,
                "MptcpStream may not have a unified peer address",
            )
        })
    }
    fn local_addr(&self) -> io::Result<SocketAddr> {
        self.0.local_addr().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::Unsupported,
                "MptcpStream may not have a unified local address",
            )
        })
    }
}

const MAX_SESSION_STREAMS: usize = 4;

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MptcpProxyServerConfig {
    pub listen_addr: Arc<str>,
    #[serde(flatten)]
    pub inner: StreamProxyConnHandlerConfig,
}
impl MptcpProxyServerConfig {
    pub fn into_builder(self, stream_context: StreamRuntime) -> MptcpProxyServerBuilder {
        let listen_addr = Arc::clone(&self.listen_addr);
        let inner = self.inner.into_builder(stream_context, listen_addr);
        MptcpProxyServerBuilder {
            listen_addr: self.listen_addr,
            inner,
        }
    }
}

#[derive(Debug, Clone)]
pub struct MptcpProxyServerBuilder {
    pub listen_addr: Arc<str>,
    pub inner: StreamProxyConnHandlerBuilder,
}
impl loading::Build for MptcpProxyServerBuilder {
    type ConnHandler = StreamProxyConnHandler;
    type Server = MptcpServer<Self::ConnHandler>;
    type Err = MptcpProxyServerBuildError;

    async fn build_server(self) -> Result<Self::Server, Self::Err> {
        let listen_addr = self.listen_addr.clone();
        let session_spawner = self.inner.stream_context.session_spawner.clone();
        let stream_proxy = self.build_conn_handler()?;
        build_mptcp_proxy_server(listen_addr.as_ref(), stream_proxy, session_spawner)
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
pub enum MptcpProxyServerBuildError {
    #[error("{0}")]
    Hook(#[from] StreamProxyServerBuildError),
    #[error("{0}")]
    Server(#[from] ListenerBindError),
}
pub async fn build_mptcp_proxy_server(
    listen_addr: impl ToSocketAddrs,
    stream_proxy: StreamProxyConnHandler,
    session_spawner: SessionSpawner,
) -> Result<MptcpServer<StreamProxyConnHandler>, ListenerBindError> {
    let listener = MptcpListener::bind(
        [listen_addr].iter(),
        NonZeroUsize::new(MAX_SESSION_STREAMS).unwrap(),
    )
    .await
    .map_err(ListenerBindError)?;
    let server = MptcpServer::new(listener, stream_proxy, session_spawner);
    Ok(server)
}
