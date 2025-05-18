use std::{
    io,
    net::SocketAddr,
    pin::Pin,
    sync::{Arc, RwLock},
};

use metrics::counter;
use thiserror::Error;
use tokio::{
    io::{AsyncRead, AsyncWrite},
    net::UdpSocket,
};
use tokio_kcp::{KcpConfig, KcpListener, KcpNoDelayConfig, KcpStream};
use tracing::{error, info, instrument, trace, warn};

use common::{
    addr::any_addr,
    connect::ConnectorConfig,
    error::AnyResult,
    loading,
    stream::{HasIoAddr, OwnIoStream, StreamServerHandleConn, connect::StreamConnect},
};

use crate::stream::connection::Conn;

#[derive(Debug)]
pub struct KcpServer<ConnHandler> {
    listener: KcpListener,
    conn_handler: ConnHandler,
}
impl<ConnHandler> KcpServer<ConnHandler> {
    pub fn new(listener: KcpListener, conn_handler: ConnHandler) -> Self {
        Self {
            listener,
            conn_handler,
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
        set_conn_handler_rx: tokio::sync::mpsc::Receiver<Self::ConnHandler>,
    ) -> AnyResult {
        self.serve_(set_conn_handler_rx).await.map_err(|e| e.into())
    }
}
impl<ConnHandler> KcpServer<ConnHandler>
where
    ConnHandler: StreamServerHandleConn + Send + Sync + 'static,
{
    #[instrument(skip(self))]
    async fn serve_(
        mut self,
        mut set_conn_handler_rx: tokio::sync::mpsc::Receiver<ConnHandler>,
    ) -> Result<(), ServeError> {
        let addr = self.listener.local_addr().map_err(ServeError::LocalAddr)?;
        info!(?addr, "Listening");
        // Arc conn_handler
        let mut conn_handler = Arc::new(self.conn_handler);
        loop {
            trace!("Waiting for connection");
            tokio::select! {
                res = self.listener.accept() => {
                    // let (stream, peer_addr) = res.map_err(|e| ServeError::Accept { source: e.into(), addr })?;
                    let (stream, peer_addr) = match res {
                        Ok(res) => res,
                        Err(e) => {
                            warn!(?e, ?addr, "Accept error");
                            continue;
                        }
                    };
                    let stream = AddressedKcpStream {
                        stream,
                        local_addr: addr,
                        peer_addr,
                    };
                    counter!("stream.kcp.accepts").increment(1);
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
pub struct KcpConnector {
    config: Arc<RwLock<ConnectorConfig>>,
}
impl KcpConnector {
    pub fn new(config: Arc<RwLock<ConnectorConfig>>) -> Self {
        Self { config }
    }
}
impl StreamConnect for KcpConnector {
    type Conn = Conn;
    async fn connect(&self, addr: SocketAddr) -> io::Result<Conn> {
        let bind = self
            .config
            .read()
            .unwrap()
            .bind
            .get_matched(&addr.ip())
            .map(|ip| SocketAddr::new(ip, 0))
            .unwrap_or_else(|| any_addr(&addr.ip()));
        let socket = UdpSocket::bind(bind).await?;
        let config = fast_kcp_config();
        let stream = KcpStream::connect_with_socket(&config, socket, addr).await?;
        let local_addr = any_addr(&addr.ip());
        let stream = AddressedKcpStream {
            stream,
            local_addr,
            peer_addr: addr,
        };
        counter!("stream.kcp.connects").increment(1);
        Ok(Conn::Kcp(stream))
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
impl OwnIoStream for AddressedKcpStream {}
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
