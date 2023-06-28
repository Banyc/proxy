use std::{
    collections::HashMap,
    fmt::Display,
    io,
    net::SocketAddr,
    ops::Deref,
    sync::{Arc, RwLock},
    time::Duration,
};

use async_trait::async_trait;
use bytesize::ByteSize;
use thiserror::Error;
use tokio::{net::UdpSocket, sync::mpsc};
use tracing::{error, info, instrument, trace, warn};

use crate::{addr::InternetAddr, error::AnyResult, loading};

pub mod header;
pub mod io_copy;
pub mod proxy_table;

pub const BUFFER_LENGTH: usize = 1024 * 8;

pub const TIMEOUT: Duration = Duration::from_secs(10);
pub const LIVE_CHECK_INTERVAL: Duration = Duration::from_secs(1);

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

#[async_trait]
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
        let mut buf = [0; BUFFER_LENGTH];
        let mut hook = Arc::new(self.hook);
        loop {
            trace!("Waiting for packet");
            tokio::select! {
                res = downstream_listener.recv_from(&mut buf) => {
                    let (n, downstream_addr) = match res {
                        Ok((n, addr)) => (n, addr),
                        Err(e) => {
                            // Ref: https://learn.microsoft.com/en-us/windows/win32/api/winsock/nf-winsock-recvfrom
                            warn!(?e, ?addr, "Failed to receive packet");
                            continue;
                        }
                    };

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
    #[error("Failed to get local address")]
    LocalAddr(#[source] io::Error),
    #[error("Failed to receive packet from downstream")]
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
    buf: &[u8],
    hook: Arc<H>,
) where
    H: UdpServerHook + Send + Sync + 'static,
{
    let (upstream, payload) = match hook.parse_upstream_addr(buf, &downstream_writer).await {
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
            tokio::spawn(async move {
                hook.handle_flow(rx, flow.clone(), downstream_writer).await;

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
pub trait UdpServerHook: loading::Hook {
    async fn parse_upstream_addr<'buf>(
        &self,
        buf: &'buf [u8],
        downstream_writer: &UdpDownstreamWriter,
    ) -> Option<(UpstreamAddr, &'buf [u8])>;

    async fn handle_flow(
        &self,
        rx: mpsc::Receiver<Packet>,
        flow: Flow,
        downstream_writer: UdpDownstreamWriter,
    );
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

pub struct Packet(pub Vec<u8>);

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct FlowMetrics {
    pub flow: Flow,
    pub start: std::time::Instant,
    pub end: std::time::Instant,
    pub bytes_uplink: u64,
    pub bytes_downlink: u64,
    pub packets_uplink: usize,
    pub packets_downlink: usize,
}

impl Display for FlowMetrics {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let duration = self.end - self.start;
        let duration = duration.as_secs_f64();
        let uplink_speed = self.bytes_uplink as f64 / duration;
        let downlink_speed = self.bytes_downlink as f64 / duration;
        write!(
            f,
            "{:.1}s,up{{{},{},{}/s}},dn{{{},{},{}/s}},up:{},dn:{}",
            duration,
            self.packets_uplink,
            ByteSize::b(self.bytes_uplink),
            ByteSize::b(uplink_speed as u64),
            self.packets_downlink,
            ByteSize::b(self.bytes_downlink),
            ByteSize::b(downlink_speed as u64),
            self.flow.upstream.0,
            self.flow.downstream.0,
        )?;
        Ok(())
    }
}
