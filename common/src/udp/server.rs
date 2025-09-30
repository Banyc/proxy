use std::{io, net::SocketAddr, num::NonZeroUsize, sync::Arc};

use thiserror::Error;
use tokio::net::UdpSocket;
use tracing::{info, instrument, trace, warn};
use udp_listener::{Conn, UtpListener};

use crate::{
    error::AnyResult,
    loading,
    proto::conn::udp::{DownstreamAddr, Flow, UpstreamAddr},
    udp::Packet,
};

#[derive(Debug)]
pub struct UdpServer<ConnHandler> {
    listener: UdpSocket,
    conn_handler: ConnHandler,
}
impl<ConnHandler> UdpServer<ConnHandler> {
    pub fn new(listener: UdpSocket, conn_handler: ConnHandler) -> Self {
        Self {
            listener,
            conn_handler,
        }
    }

    pub fn listener(&self) -> &UdpSocket {
        &self.listener
    }

    pub fn listener_mut(&mut self) -> &mut UdpSocket {
        &mut self.listener
    }
}
impl<ConnHandler> loading::Serve for UdpServer<ConnHandler>
where
    ConnHandler: UdpServerHandleConn + Send + Sync + 'static,
{
    type ConnHandler = ConnHandler;

    async fn serve(
        self,
        set_conn_handler_rx: loading::ReplaceConnHandlerRx<Self::ConnHandler>,
    ) -> AnyResult {
        self.serve_(set_conn_handler_rx).await.map_err(|e| e.into())
    }
}
impl<ConnHandler> UdpServer<ConnHandler>
where
    ConnHandler: UdpServerHandleConn + Send + Sync + 'static,
{
    #[instrument(skip(self))]
    async fn serve_(
        self,
        mut set_conn_handler_rx: loading::ReplaceConnHandlerRx<ConnHandler>,
    ) -> Result<(), ServeError> {
        let mut conn_handler = Arc::new(self.conn_handler);

        let dispatch = {
            let conn_handler = conn_handler.clone();
            move |&addr: &SocketAddr, packet: udp_listener::Packet| -> Option<(Flow, Packet)> {
                let conn_handler = conn_handler.clone();
                let mut buf_reader = io::Cursor::new(&packet[..]);
                let upstream_addr = conn_handler.parse_upstream_addr(&mut buf_reader)?;
                let flow = Flow {
                    upstream: upstream_addr,
                    downstream: DownstreamAddr(addr),
                };

                let read = buf_reader.position() as usize;
                let mut packet = Packet::new(packet);
                packet.advance(read).unwrap();

                Some((flow, packet))
            }
        };

        let addr = self.listener.local_addr().map_err(ServeError::LocalAddr)?;

        let dispatcher_buffer_size = NonZeroUsize::new(64).unwrap();
        let downstream_listener =
            UtpListener::new(self.listener, dispatcher_buffer_size, Arc::new(dispatch));

        info!(?addr, "Listening");
        let mut warned = false;
        loop {
            trace!("Waiting for packet");
            tokio::select! {
                res = downstream_listener.accept() => {
                    let flow = match res {
                        Ok(x) => x,
                        Err(e) => {
                            // Ref: https://learn.microsoft.com/en-us/windows/win32/api/winsock/nf-winsock-recvfrom
                            if !warned {
                                warn!(?e, ?addr, "Failed to receive packet");
                            }
                            warned = true;
                            continue;
                        }
                    };
                    warned = false;

                    let conn_handler = Arc::clone(&conn_handler);
                    tokio::spawn(async move {
                        conn_handler.handle_flow(flow).await;
                    });
                }
                res = set_conn_handler_rx.0.recv() => {
                    let new_hook = match res {
                        Some(new_hook) => new_hook,
                        None => break,
                    };
                    info!(?addr, "Connection handler set");
                    conn_handler = Arc::new(new_hook);
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
    #[error("Failed to receive packet from downstream: {source}, {addr}")]
    RecvFrom {
        #[source]
        source: io::Error,
        addr: SocketAddr,
    },
}

pub trait UdpServerHandleConn: loading::HandleConn {
    fn parse_upstream_addr(&self, buf: &mut io::Cursor<&[u8]>) -> Option<Option<UpstreamAddr>>;

    fn handle_flow(&self, conn: Conn<UdpSocket, Flow, Packet>) -> impl Future<Output = ()> + Send;
}
