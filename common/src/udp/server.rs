use std::{
    io,
    net::SocketAddr,
    num::NonZeroUsize,
    sync::{Arc, RwLock},
};

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
    #[instrument(skip_all)]
    async fn serve_(
        self,
        mut set_conn_handler_rx: loading::ReplaceConnHandlerRx<ConnHandler>,
    ) -> Result<(), ServeError> {
        let conn_handler = Arc::new(RwLock::new(Arc::new(self.conn_handler)));
        let dispatch = {
            let conn_handler = Arc::clone(&conn_handler);
            move |&addr: &SocketAddr, packet: udp_listener::Packet| -> Option<(Flow, Packet)> {
                let conn_handler = Arc::clone(&conn_handler.read().unwrap());
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
                            if !warned {
                                warn!(?e, ?addr, "Failed to receive packet");
                            }
                            warned = true;
                            continue;
                        }
                    };
                    warned = false;
                    let conn_handler = Arc::clone(&conn_handler.read().unwrap());
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
                    *conn_handler.write().unwrap() = Arc::new(new_hook);
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::loading::Serve;
    use std::time::Duration;
    #[derive(Debug)]
    struct TagEcho(u8);
    impl loading::HandleConn for TagEcho {}
    impl UdpServerHandleConn for TagEcho {
        fn parse_upstream_addr(&self, buf: &mut io::Cursor<&[u8]>) -> Option<Option<UpstreamAddr>> {
            let mut tag = [0; 1];
            std::io::Read::read_exact(buf, &mut tag).ok()?;
            if tag[0] != self.0 {
                return None;
            }
            Some(None)
        }
        async fn handle_flow(&self, conn: Conn<UdpSocket, Flow, Packet>) {
            let (mut read, write) = conn.split();
            while let Some(packet) = read.recv().recv().await {
                let _ = write.send(packet.slice()).await;
            }
        }
    }
    #[tokio::test]
    async fn a_hot_reload_reaches_the_dispatcher() {
        let listener = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (set_conn_handler_tx, set_conn_handler_rx) = loading::replace_conn_handler_channel();
        tokio::spawn(UdpServer::new(listener, TagEcho(1)).serve(set_conn_handler_rx));
        let echoed = |tag: u8| async move {
            let client = UdpSocket::bind("127.0.0.1:0").await.unwrap();
            for _ in 0..50 {
                client.send_to(&[tag, 42], addr).await.unwrap();
                let mut buf = [0; 8];
                let recv = tokio::time::timeout(Duration::from_millis(100), client.recv(&mut buf));
                if let Ok(Ok(n)) = recv.await {
                    return Some(buf[..n].to_vec());
                }
            }
            None
        };
        assert_eq!(echoed(1).await.as_deref(), Some(&[42][..]));
        set_conn_handler_tx.0.send(TagEcho(2)).await.unwrap();
        assert_eq!(echoed(2).await.as_deref(), Some(&[42][..]));
    }
}
