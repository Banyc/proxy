use std::{
    io,
    net::SocketAddr,
    pin::Pin,
    sync::{Arc, RwLock},
};

use metrics::counter;
use thiserror::Error;
use tokio::io::{AsyncRead, AsyncWrite};
use tracing::{error, info, instrument, trace, warn};

use common::{
    addr::any_addr,
    connect::ConnectorConfig,
    error::AnyResult,
    loading,
    stream::{connect::StreamConnect, IoAddr, IoStream, StreamServerHandleConn},
};

use crate::stream::connection::Connection;

#[derive(Debug)]
pub struct RtpServer<ConnHandler> {
    listener: rtp::udp::Listener,
    conn_handler: ConnHandler,
}
impl<ConnHandler> RtpServer<ConnHandler> {
    pub fn new(listener: rtp::udp::Listener, conn_handler: ConnHandler) -> Self {
        Self {
            listener,
            conn_handler,
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
                res = self.listener.accept_without_handshake() => {
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
}
impl RtpConnector {
    pub fn new(config: Arc<RwLock<ConnectorConfig>>) -> Self {
        Self { config }
    }
}
impl StreamConnect for RtpConnector {
    type Connection = Connection;
    async fn connect(&self, addr: SocketAddr) -> io::Result<Connection> {
        let bind = self
            .config
            .read()
            .unwrap()
            .bind
            .get_matched(&addr.ip())
            .map(|ip| SocketAddr::new(ip, 0))
            .unwrap_or_else(|| any_addr(&addr.ip()));
        let connected = rtp::udp::connect_without_handshake(bind, addr, None).await?;
        let stream = AddressedRtpStream {
            read: connected.read.into_async_read(),
            write: connected.write.into_async_write(),
            local_addr: connected.local_addr,
            peer_addr: connected.peer_addr,
        };
        counter!("stream.rtp.connects").increment(1);
        Ok(Connection::Rtp(stream))
    }
}

#[derive(Debug)]
pub struct AddressedRtpStream {
    read: rtp::socket::ReadStream,
    write: rtp::socket::WriteStream,
    local_addr: SocketAddr,
    peer_addr: SocketAddr,
}
impl IoStream for AddressedRtpStream {}
impl IoAddr for AddressedRtpStream {
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
