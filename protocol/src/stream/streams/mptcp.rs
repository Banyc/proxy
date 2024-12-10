use std::{io, net::SocketAddr, sync::Arc};

use metrics::counter;
use mptcp::{listen::MptcpListener, stream::MptcpStream};
use thiserror::Error;
use tokio::io::{AsyncRead, AsyncWrite};
use tracing::{info, instrument, trace, warn};

use common::{
    error::AnyResult,
    loading,
    stream::{connect::StreamConnect, IoAddr, IoStream, StreamServerHandleConn},
};

use crate::stream::connection::Connection;

const STREAMS: usize = 4;

#[derive(Debug)]
pub struct MptcpServer<ConnHandler> {
    listener: MptcpListener,
    conn_handler: ConnHandler,
}
impl<ConnHandler> MptcpServer<ConnHandler> {
    pub fn new(listener: MptcpListener, conn_handler: ConnHandler) -> Self {
        Self {
            listener,
            conn_handler,
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
        set_conn_handler_rx: tokio::sync::mpsc::Receiver<Self::ConnHandler>,
    ) -> AnyResult {
        self.serve_(set_conn_handler_rx).await.map_err(|e| e.into())
    }
}
impl<ConnHandler> MptcpServer<ConnHandler>
where
    ConnHandler: StreamServerHandleConn + Send + Sync + 'static,
{
    #[instrument(skip(self))]
    async fn serve_(
        mut self,
        mut set_conn_handler_rx: tokio::sync::mpsc::Receiver<ConnHandler>,
    ) -> Result<(), ServeError> {
        let addr = self
            .listener
            .local_addrs()
            .next()
            .unwrap()
            .map_err(ServeError::LocalAddr)?;
        info!(?addr, "Listening");
        // Arc conn_handler
        let mut conn_handler = Arc::new(self.conn_handler);
        loop {
            trace!("Waiting for connection");
            tokio::select! {
                res = self.listener.accept() => {
                    let stream = match res {
                        Ok(res) => res,
                        Err(e) => {
                            warn!(?e, ?addr, "Accept error");
                            continue;
                        }
                    };
                    counter!("stream.mptcp.accepts").increment(1);
                    // Arc conn_handler
                    let conn_handler = Arc::clone(&conn_handler);
                    tokio::spawn(async move {
                        conn_handler.handle_stream(IoMptcpStream(stream)).await;
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

#[derive(Debug, Clone, Copy)]
pub struct MptcpConnector;
impl StreamConnect for MptcpConnector {
    type Connection = Connection;
    async fn connect(&self, addr: SocketAddr) -> io::Result<Connection> {
        let addrs = core::iter::repeat(()).take(STREAMS).map(|()| addr);
        let stream = MptcpStream::connect(addrs).await?;
        counter!("stream.mptcp.connects").increment(1);
        Ok(Connection::Mptcp(IoMptcpStream(stream)))
    }
}

#[derive(Debug)]
pub struct IoMptcpStream(pub MptcpStream);
impl AsyncWrite for IoMptcpStream {
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
impl AsyncRead for IoMptcpStream {
    fn poll_read(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<io::Result<()>> {
        std::pin::Pin::new(&mut self.0).poll_read(cx, buf)
    }
}
impl IoStream for IoMptcpStream {}
impl IoAddr for IoMptcpStream {
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
