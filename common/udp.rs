use std::{
    collections::HashMap,
    io,
    net::SocketAddr,
    ops::Deref,
    sync::{Arc, RwLock},
};

use async_trait::async_trait;
use tokio::{net::UdpSocket, sync::mpsc};
use tracing::{error, info, instrument, trace};

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

    pub fn remote_addr(&self) -> SocketAddr {
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
}

impl<H> UdpServer<H> {
    pub fn new(listener: UdpSocket, hook: H) -> Self {
        Self { listener, hook }
    }

    pub fn listener(&self) -> &UdpSocket {
        &self.listener
    }

    pub fn listener_mut(&mut self) -> &mut UdpSocket {
        &mut self.listener
    }
}

impl<H> UdpServer<H>
where
    H: UdpServerHook + Send + Sync + 'static,
{
    #[instrument(skip(self))]
    pub async fn serve(self) -> io::Result<()> {
        let flows: FlowMap = HashMap::new();
        let flows = Arc::new(RwLock::new(flows));
        let downstream_listener = Arc::new(self.listener);

        let addr = downstream_listener
            .local_addr()
            .inspect_err(|e| error!(?e, "Failed to get local address"))?;
        info!(?addr, "Listening");
        let mut buf = [0; 1024];
        let hook = Arc::new(self.hook);
        loop {
            trace!("Waiting for packet");
            let (n, downstream_addr) = downstream_listener
                .recv_from(&mut buf)
                .await
                .inspect_err(|e| error!(?e, "Failed to receive packet from downstream"))?;
            let downstream_writer =
                UdpDownstreamWriter::new(Arc::clone(&downstream_listener), downstream_addr);

            steer(
                downstream_writer.clone(),
                Arc::clone(&flows),
                &buf[..n],
                Arc::clone(&hook),
            )
            .await;
        }
    }
}

#[instrument(skip(downstream_writer, flows, buf, hook))]
async fn steer<H>(
    downstream_writer: UdpDownstreamWriter,
    flows: Arc<RwLock<FlowMap>>,
    buf: &[u8],
    hook: Arc<H>,
) where
    H: UdpServerHook + Send + Sync + 'static,
{
    let (upstream, payload) = match hook.parse_upstream_addr(buf, &downstream_writer).await {
        Ok(x) => x,
        Err(e) => {
            error!(?e, "Failed to parse upstream address");
            return;
        }
    };

    // Create flow if not exists
    let flow = Flow {
        upstream,
        downstream: DownstreamAddr(downstream_writer.remote_addr()),
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
            let (tx, rx) = mpsc::channel(1);
            flows.write().unwrap().insert(flow, tx.clone());

            let hook = Arc::clone(&hook);
            tokio::spawn(async move {
                hook.handle_flow(rx, flow, downstream_writer).await;

                // Remove flow
                flows.write().unwrap().remove(&flow);
            });

            tx
        }
    };

    // Steer packet
    let packet = Packet(payload.to_vec());
    let _ = flow_tx.send(packet).await;
}

#[async_trait]
pub trait UdpServerHook {
    async fn parse_upstream_addr<'buf>(
        &self,
        buf: &'buf [u8],
        downstream_writer: &UdpDownstreamWriter,
    ) -> Result<(UpstreamAddr, &'buf [u8]), ()>;

    async fn handle_flow(
        &self,
        rx: mpsc::Receiver<Packet>,
        flow: Flow,
        downstream_writer: UdpDownstreamWriter,
    );
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct DownstreamAddr(pub SocketAddr);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct UpstreamAddr(pub SocketAddr);

type FlowMap = HashMap<Flow, mpsc::Sender<Packet>>;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Flow {
    pub upstream: UpstreamAddr,
    pub downstream: DownstreamAddr,
}

pub struct Packet(pub Vec<u8>);
