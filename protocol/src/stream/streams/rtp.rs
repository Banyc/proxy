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
use tracing::{error, info, instrument, trace, warn};

use common::{
    addr::any_addr,
    connect::ConnectorConfig,
    error::AnyResult,
    loading,
    proto::{
        connect::stream::StreamConnect,
        context::StreamContext,
        server::{
            ListenerBindError,
            stream::{
                StreamProxyConnHandler, StreamProxyConnHandlerBuilder, StreamProxyServerBuildError,
                StreamProxyServerConfig,
            },
        },
    },
    stream::{AsConn, HasIoAddr, OwnIoStream, StreamServerHandleConn},
};

#[derive(Debug)]
pub struct RtpServer<ConnHandler> {
    listener: rtp::udp::Listener,
    conn_handler: ConnHandler,
    fec: bool,
}
impl<ConnHandler> RtpServer<ConnHandler> {
    pub fn new(listener: rtp::udp::Listener, conn_handler: ConnHandler, fec: bool) -> Self {
        Self {
            listener,
            conn_handler,
            fec,
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
        set_conn_handler_rx: tokio::sync::mpsc::Receiver<Self::ConnHandler>,
    ) -> AnyResult {
        self.serve_(set_conn_handler_rx).await.map_err(|e| e.into())
    }
}
impl<ConnHandler> RtpServer<ConnHandler>
where
    ConnHandler: StreamServerHandleConn + Send + Sync + 'static,
{
    #[instrument(skip(self))]
    async fn serve_(
        self,
        mut set_conn_handler_rx: tokio::sync::mpsc::Receiver<ConnHandler>,
    ) -> Result<(), ServeError> {
        let addr = self.listener.local_addr();
        info!(?addr, "Listening");
        // Arc conn_handler
        let mut conn_handler = Arc::new(self.conn_handler);
        loop {
            trace!("Waiting for connection");
            tokio::select! {
                res = self.listener.accept_without_handshake(self.fec) => {
                    let stream = match res {
                        Ok(res) => res,
                        Err(e) => {
                            warn!(?e, ?addr, "Accept error");
                            continue;
                        }
                    };
                    let stream = AddressedRtpStream {
                        read: stream.read.into_async_read(),
                        write: stream.write.into_async_write(),
                        local_addr: self.listener.local_addr(),
                        peer_addr: stream.peer_addr,
                    };
                    counter!("stream.rtp.accepts").increment(1);
                    // Arc conn_handler
                    let conn_handler = Arc::clone(&conn_handler);
                    tokio::spawn(async move {
                        conn_handler.handle_stream(stream).await;
                    });
                }
                res = set_conn_handler_rx.recv() => {
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
    #[error("Failed to accept connection: {source}, {addr}")]
    Accept {
        #[source]
        source: io::Error,
        addr: SocketAddr,
    },
}

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
    async fn connect(&self, addr: SocketAddr) -> io::Result<Box<dyn AsConn>> {
        let bind = self
            .config
            .read()
            .unwrap()
            .bind
            .get_matched(&addr.ip())
            .map(|ip| SocketAddr::new(ip, 0))
            .unwrap_or_else(|| any_addr(&addr.ip()));
        let connected = rtp::udp::connect_without_handshake(bind, addr, None, self.fec).await?;
        let stream = AddressedRtpStream {
            read: connected.read.into_async_read(),
            write: connected.write.into_async_write(),
            local_addr: connected.local_addr,
            peer_addr: connected.peer_addr,
        };
        counter!("stream.rtp.connects").increment(1);
        Ok(Box::new(stream))
    }
}

#[derive(Debug)]
pub struct AddressedRtpStream {
    read: rtp::socket::ReadStream,
    write: rtp::socket::WriteStream,
    local_addr: SocketAddr,
    peer_addr: SocketAddr,
}
impl AsConn for AddressedRtpStream {}
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
    pub inner: StreamProxyServerConfig,
}
impl RtpProxyServerConfig {
    pub fn into_builder(self, stream_context: StreamContext) -> RtpProxyServerBuilder {
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
        let stream_proxy = self.build_conn_handler()?;
        build_rtp_proxy_server(listen_addr.as_ref(), stream_proxy)
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
) -> Result<RtpServer<StreamProxyConnHandler>, ListenerBindError> {
    let fec = false;
    let listener = rtp::udp::Listener::bind(listen_addr)
        .await
        .map_err(ListenerBindError)?;
    let server = RtpServer::new(listener, stream_proxy, fec);
    Ok(server)
}
