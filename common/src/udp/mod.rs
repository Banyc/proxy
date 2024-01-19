use std::{
    collections::HashMap,
    io,
    net::SocketAddr,
    ops::Deref,
    sync::{Arc, RwLock},
    time::Duration,
};

use bytes::BytesMut;
use lockfree_object_pool::{LinearObjectPool, LinearOwnedReusable};
use once_cell::sync::Lazy;
use thiserror::Error;
use tokio::{net::UdpSocket, sync::mpsc};
use tracing::{error, info, instrument, trace, warn};

use crate::{addr::InternetAddr, error::AnyResult, loading};

pub mod context;
pub mod header;
pub mod io_copy;
pub mod metrics;
pub mod proxy_table;
pub mod respond;
pub mod session_table;
pub mod steer;

pub const BUFFER_LENGTH: usize = 2_usize.pow(16);
pub static BUFFER_POOL: Lazy<Arc<LinearObjectPool<BytesMut>>> = Lazy::new(|| {
    Arc::new(LinearObjectPool::new(
        || BytesMut::with_capacity(BUFFER_LENGTH),
        |buf| buf.clear(),
    ))
});

pub const TIMEOUT: Duration = Duration::from_secs(10);
pub const ACTIVITY_CHECK_INTERVAL: Duration = Duration::from_secs(1);

#[derive(Debug, Clone)]
pub struct UdpDownstreamWriter {
    downstream_writer: Arc<UdpSocket>,
    downstream_addr: SocketAddr,
}

impl UdpDownstreamWriter {
    pub fn new(downstream_writer: Arc<UdpSocket>, downstream_addr: SocketAddr) -> Self {
        Self {
            downstream_writer,
            downstream_addr,
        }
    }

    pub async fn send(&self, buf: &[u8]) -> io::Result<usize> {
        self.downstream_writer
            .send_to(buf, self.downstream_addr)
            .await
    }

    pub fn peer_addr(&self) -> SocketAddr {
        self.downstream_addr
    }
}

impl Deref for UdpDownstreamWriter {
    type Target = UdpSocket;

    fn deref(&self) -> &Self::Target {
        &self.downstream_writer
    }
}

#[derive(Debug)]
pub struct UdpServer<H> {
    listener: UdpSocket,
    hook: H,
    handle: mpsc::Sender<H>,
    set_hook_rx: mpsc::Receiver<H>,
}

impl<H> UdpServer<H> {
    pub fn new(listener: UdpSocket, hook: H) -> Self {
        let (set_hook_tx, set_hook_rx) = mpsc::channel(64);
        Self {
            listener,
            hook,
            handle: set_hook_tx,
            set_hook_rx,
        }
    }

    pub fn listener(&self) -> &UdpSocket {
        &self.listener
    }

    pub fn listener_mut(&mut self) -> &mut UdpSocket {
        &mut self.listener
    }
}

impl<H> loading::Server for UdpServer<H>
where
    H: UdpServerHook + Send + Sync + 'static,
{
    type Hook = H;

    fn handle(&self) -> mpsc::Sender<Self::Hook> {
        self.handle.clone()
    }

    async fn serve(self) -> AnyResult {
        self.serve_().await.map_err(|e| e.into())
    }
}

impl<H> UdpServer<H>
where
    H: UdpServerHook + Send + Sync + 'static,
{
    #[instrument(skip(self))]
    async fn serve_(mut self) -> Result<(), ServeError> {
        drop(self.handle);

        let flows: FlowMap = HashMap::new();
        let flows = Arc::new(RwLock::new(flows));
        let downstream_listener = Arc::new(self.listener);

        let addr = downstream_listener
            .local_addr()
            .map_err(ServeError::LocalAddr)?;
        info!(?addr, "Listening");
        let mut hook = Arc::new(self.hook);
        let mut warned = false;
        loop {
            trace!("Waiting for packet");
            let mut buf = BUFFER_POOL.pull_owned();
            tokio::select! {
                res = downstream_listener.recv_buf_from(&mut *buf) => {
                    let (n, downstream_addr) = match res {
                        Ok((n, addr)) => (n, addr),
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
                    if n == BUFFER_LENGTH {
                        warn!(?addr, ?n, "Received uplink packet of size may be too large");
                        continue;
                    }

                    let downstream_writer =
                        UdpDownstreamWriter::new(Arc::clone(&downstream_listener), downstream_addr);
                    steer(
                        downstream_writer.clone(),
                        Arc::clone(&flows),
                        buf,
                        Arc::clone(&hook),
                    )
                    .await;
                }
                res = self.set_hook_rx.recv() => {
                    let new_hook = match res {
                        Some(new_hook) => new_hook,
                        None => break,
                    };
                    info!(?addr, "Hook set");
                    hook = Arc::new(new_hook);
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

#[instrument(skip(downstream_writer, flows, buf, hook))]
async fn steer<H>(
    downstream_writer: UdpDownstreamWriter,
    flows: Arc<RwLock<FlowMap>>,
    buf: LinearOwnedReusable<BytesMut>,
    hook: Arc<H>,
) where
    H: UdpServerHook + Send + Sync + 'static,
{
    let mut buf_reader = io::Cursor::new(&buf[..]);
    let upstream = match hook
        .parse_upstream_addr(&mut buf_reader, &downstream_writer)
        .await
    {
        Some(x) => x,
        None => {
            trace!("No upstream address found");
            return;
        }
    };

    // Create flow if not exists
    let flow = Flow {
        upstream,
        downstream: DownstreamAddr(downstream_writer.peer_addr()),
    };
    let flow_tx = {
        let flows = flows.read().unwrap();
        flows.get(&flow).cloned()
    };
    let flow_tx = match flow_tx {
        Some(flow_tx) => {
            trace!(?flow, "Flow already exists");
            flow_tx
        }
        None => {
            trace!(?flow, "Creating flow");
            let (tx, rx) = mpsc::channel(64);
            flows.write().unwrap().insert(flow.clone(), tx.clone());

            let hook = Arc::clone(&hook);
            let flow = FlowOwnedGuard {
                flow: flow.clone(),
                map: Arc::clone(&flows),
            };
            tokio::spawn(async move {
                hook.handle_flow(rx, flow, downstream_writer).await;
            });

            tx
        }
    };

    // Steer packet
    let read = buf_reader.position() as usize;
    let mut packet = Packet::new(buf);
    packet.advance(read).unwrap();
    let _ = flow_tx.send(packet).await;
}

pub struct FlowOwnedGuard {
    flow: Flow,
    map: Arc<RwLock<FlowMap>>,
}

impl FlowOwnedGuard {
    pub fn flow(&self) -> &Flow {
        &self.flow
    }
}

impl Drop for FlowOwnedGuard {
    fn drop(&mut self) {
        // Remove flow
        self.map.write().unwrap().remove(&self.flow);
    }
}

pub trait UdpServerHook: loading::Hook {
    fn parse_upstream_addr(
        &self,
        buf: &mut io::Cursor<&[u8]>,
        downstream_writer: &UdpDownstreamWriter,
    ) -> impl std::future::Future<Output = Option<UpstreamAddr>> + Send;

    fn handle_flow(
        &self,
        rx: mpsc::Receiver<Packet>,
        flow: FlowOwnedGuard,
        downstream_writer: UdpDownstreamWriter,
    ) -> impl std::future::Future<Output = ()> + Send;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct DownstreamAddr(pub SocketAddr);

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct UpstreamAddr(pub InternetAddr);

type FlowMap = HashMap<Flow, mpsc::Sender<Packet>>;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Flow {
    pub upstream: UpstreamAddr,
    pub downstream: DownstreamAddr,
}

pub struct Packet {
    buf: LinearOwnedReusable<BytesMut>,
    pos: usize,
}

impl Packet {
    pub fn new(buf: LinearOwnedReusable<BytesMut>) -> Self {
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
