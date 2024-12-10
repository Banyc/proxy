use std::{io, net::SocketAddr, num::NonZeroUsize, sync::Arc, time::Duration};

use hdv_derive::HdvSerde;
use thiserror::Error;
use tokio::{net::UdpSocket, sync::mpsc};
use tracing::{error, info, instrument, trace, warn};
use udp_listener::{Conn, UtpListener};

use crate::{
    addr::{InternetAddr, InternetAddrHdv},
    error::AnyResult,
    loading,
};

pub mod context;
pub mod header;
pub mod io_copy;
pub mod log;
pub mod metrics;
pub mod proxy_table;
pub mod respond;
pub mod steer;

pub const PACKET_BUFFER_LENGTH: usize = 2_usize.pow(16);
pub const TIMEOUT: Duration = Duration::from_secs(10);
pub const ACTIVITY_CHECK_INTERVAL: Duration = Duration::from_secs(1);

#[derive(Debug)]
pub struct UdpServer<ConnHandler> {
    listener: UdpSocket,
    conn_handler: ConnHandler,
    handle: mpsc::Sender<ConnHandler>,
    set_conn_handler_rx: mpsc::Receiver<ConnHandler>,
}
impl<ConnHandler> UdpServer<ConnHandler> {
    pub fn new(listener: UdpSocket, conn_handler: ConnHandler) -> Self {
        let (set_conn_handler_tx, set_conn_handler_rx) = mpsc::channel(64);
        Self {
            listener,
            conn_handler,
            handle: set_conn_handler_tx,
            set_conn_handler_rx,
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

    fn handle(&self) -> mpsc::Sender<Self::ConnHandler> {
        self.handle.clone()
    }

    async fn serve(self) -> AnyResult {
        self.serve_().await.map_err(|e| e.into())
    }
}
impl<ConnHandler> UdpServer<ConnHandler>
where
    ConnHandler: UdpServerHandleConn + Send + Sync + 'static,
{
    #[instrument(skip(self))]
    async fn serve_(mut self) -> Result<(), ServeError> {
        drop(self.handle);

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
                res = self.set_conn_handler_rx.recv() => {
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

    fn handle_flow(
        &self,
        conn: Conn<UdpSocket, Flow, Packet>,
    ) -> impl std::future::Future<Output = ()> + Send;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct DownstreamAddr(pub SocketAddr);

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct UpstreamAddr(pub InternetAddr);

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Flow {
    pub upstream: Option<UpstreamAddr>,
    pub downstream: DownstreamAddr,
}
#[derive(Debug, Clone, HdvSerde)]
pub struct FlowHdv {
    pub upstream: Option<InternetAddrHdv>,
    pub downstream: InternetAddrHdv,
}
impl From<&Flow> for FlowHdv {
    fn from(value: &Flow) -> Self {
        let upstream = value.upstream.as_ref().map(|x| (&x.0).into());
        let downstream = value.downstream.0.into();
        Self {
            upstream,
            downstream,
        }
    }
}

pub struct Packet {
    buf: udp_listener::Packet,
    pos: usize,
}

impl Packet {
    pub fn new(buf: udp_listener::Packet) -> Self {
        Self { buf, pos: 0 }
    }

    pub fn advance(&mut self, bytes: usize) -> Result<(), PacketPositionAdvancesOutOfRange> {
        let new_pos = bytes + self.pos;
        if new_pos > self.buf.len() {
            return Err(PacketPositionAdvancesOutOfRange);
        }
        self.pos = new_pos;
        Ok(())
    }

    pub fn slice(&self) -> &[u8] {
        &self.buf[self.pos..]
    }

    pub fn slice_mut(&mut self) -> &mut [u8] {
        &mut self.buf[self.pos..]
    }
}

#[derive(Debug, Error)]
#[error("Packet position advances out of range")]
pub struct PacketPositionAdvancesOutOfRange;
