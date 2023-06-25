use std::{
    collections::HashMap,
    io,
    net::Ipv4Addr,
    sync::{Arc, RwLock},
};

use async_speed_limit::Limiter;
use async_trait::async_trait;
use common::{
    addr::any_addr,
    addr::InternetAddr,
    config::SharableConfig,
    loading,
    udp::{
        proxy_table::UdpProxyTable, Flow, FlowMetrics, Packet, UdpDownstreamWriter, UdpServer,
        UdpServerHook, UpstreamAddr, BUFFER_LENGTH, LIVE_CHECK_INTERVAL, TIMEOUT,
    },
};
use proxy_client::udp::{EstablishError, RecvError, SendError, UdpProxyClient};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::{net::ToSocketAddrs, sync::mpsc};
use tracing::{error, info, trace, warn};

use self::proxy_table::UdpProxyTableBuilder;

pub mod proxy_table;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UdpAccessServerConfig {
    pub listen_addr: Arc<str>,
    pub destination: Arc<str>,
    pub proxy_table: SharableConfig<UdpProxyTableBuilder>,
    pub speed_limit: Option<f64>,
}

impl UdpAccessServerConfig {
    pub fn into_builder(
        self,
        proxy_tables: &HashMap<Arc<str>, UdpProxyTable>,
    ) -> Result<UdpAccessServerBuilder, BuildError> {
        let proxy_table = match self.proxy_table {
            SharableConfig::SharingKey(key) => proxy_tables
                .get(&key)
                .ok_or_else(|| BuildError::ProxyTableKeyNotFound(key.clone()))?
                .clone(),
            SharableConfig::Private(x) => x.build(),
        };

        Ok(UdpAccessServerBuilder {
            listen_addr: self.listen_addr,
            destination: self.destination,
            proxy_table,
            speed_limit: self.speed_limit.unwrap_or(f64::INFINITY),
        })
    }
}

#[derive(Debug, Error)]
pub enum BuildError {
    #[error("Proxy table key not found: {0}")]
    ProxyTableKeyNotFound(Arc<str>),
}

#[derive(Debug, Clone)]
pub struct UdpAccessServerBuilder {
    listen_addr: Arc<str>,
    destination: Arc<str>,
    proxy_table: UdpProxyTable,
    speed_limit: f64,
}

#[async_trait]
impl loading::Builder for UdpAccessServerBuilder {
    type Hook = UdpAccess;
    type Server = UdpServer<Self::Hook>;

    async fn build_server(self) -> io::Result<UdpServer<UdpAccess>> {
        let listen_addr = self.listen_addr.clone();
        let access = self.build_hook()?;
        let server = access.build(listen_addr.as_ref()).await?;
        Ok(server)
    }

    fn key(&self) -> &Arc<str> {
        &self.listen_addr
    }

    fn build_hook(self) -> io::Result<UdpAccess> {
        Ok(UdpAccess::new(
            self.proxy_table,
            self.destination.into(),
            self.speed_limit,
        ))
    }
}

#[derive(Debug)]
pub struct UdpAccess {
    proxy_table: UdpProxyTable,
    destination: InternetAddr,
    speed_limit: f64,
}

impl loading::Hook for UdpAccess {}

impl UdpAccess {
    pub fn new(proxy_table: UdpProxyTable, destination: InternetAddr, speed_limit: f64) -> Self {
        Self {
            proxy_table,
            destination,
            speed_limit,
        }
    }

    pub async fn build(self, listen_addr: impl ToSocketAddrs) -> io::Result<UdpServer<Self>> {
        let listener = tokio::net::UdpSocket::bind(listen_addr)
            .await
            .map_err(|e| {
                error!(?e, "Failed to bind to listen address");
                e
            })?;
        Ok(UdpServer::new(listener, self))
    }

    async fn proxy(
        &self,
        mut rx: mpsc::Receiver<Packet>,
        flow: Flow,
        downstream_writer: UdpDownstreamWriter,
    ) -> Result<FlowMetrics, ProxyError> {
        let start = std::time::Instant::now();

        // Connect to upstream
        let proxy_chain = self.proxy_table.choose_chain();
        let upstream =
            UdpProxyClient::establish(proxy_chain.chain.clone(), self.destination.clone()).await?;

        // Periodic check if the flow is still alive
        let mut tick = tokio::time::interval(LIVE_CHECK_INTERVAL);
        let last_uplink_packet = Arc::new(RwLock::new(std::time::Instant::now()));
        let last_downlink_packet = Arc::new(RwLock::new(std::time::Instant::now()));

        let bytes_uplink = Arc::new(RwLock::new(0));
        let bytes_downlink = Arc::new(RwLock::new(0));
        let packets_uplink = Arc::new(RwLock::new(0));
        let packets_downlink = Arc::new(RwLock::new(0));

        // Limit speed
        let limiter = <Limiter>::new(self.speed_limit);

        let (mut upstream_read, mut upstream_write) = upstream.into_split();
        let mut join_set = tokio::task::JoinSet::<Result<(), ProxyError>>::new();
        join_set.spawn({
            let last_uplink_packet = Arc::clone(&last_uplink_packet);
            let bytes_uplink = Arc::clone(&bytes_uplink);
            let packets_uplink = Arc::clone(&packets_uplink);
            let limiter = limiter.clone();
            async move {
                loop {
                    let res = rx.recv().await;
                    trace!("Received packet from downstream");
                    let packet = match res {
                        Some(packet) => packet,
                        None => {
                            // Channel closed
                            break;
                        }
                    };

                    // Limit speed
                    limiter.consume(packet.0.len()).await;

                    // Send packet to upstream
                    upstream_write.send(&packet.0).await?;

                    *bytes_uplink.write().unwrap() += packet.0.len() as u64;
                    *packets_uplink.write().unwrap() += 1;
                    *last_uplink_packet.write().unwrap() = std::time::Instant::now();
                }
                Ok(())
            }
        });
        join_set.spawn({
            let last_downlink_packet = Arc::clone(&last_downlink_packet);
            let bytes_downlink = Arc::clone(&bytes_downlink);
            let packets_downlink = Arc::clone(&packets_downlink);
            let mut downlink_buf = [0; BUFFER_LENGTH];
            async move {
                loop {
                    let res = upstream_read.recv(&mut downlink_buf).await;
                    trace!("Received packet from upstream");
                    let n = res?;
                    let pkt = &downlink_buf[..n];

                    // Limit speed
                    limiter.consume(pkt.len()).await;

                    // Send packet to downstream
                    downstream_writer
                        .send(pkt)
                        .await
                        .map_err(|e| ProxyError::SendDownstream {
                            source: e,
                            downstream: downstream_writer.clone(),
                        })?;

                    *bytes_downlink.write().unwrap() += pkt.len() as u64;
                    *packets_downlink.write().unwrap() += 1;
                    *last_downlink_packet.write().unwrap() = std::time::Instant::now();
                }
            }
        });

        // Forward packets
        loop {
            trace!("Waiting for packet");
            tokio::select! {
                res = join_set.join_next() => {
                    let res = match res {
                        Some(res) => res.unwrap(),
                        None => break,
                    };
                    res?;
                }
                _ = tick.tick() => {
                    trace!("Checking if flow is still alive");
                    let now = std::time::Instant::now();
                    let last_uplink_packet = *last_uplink_packet.read().unwrap();
                    let last_downlink_packet = *last_downlink_packet.read().unwrap();

                    if now.duration_since(last_uplink_packet) > TIMEOUT && now.duration_since(last_downlink_packet) > TIMEOUT {
                        trace!(?flow, "Flow timed out");
                        break;
                    }
                }
            }
        }
        join_set.abort_all();

        let last_packet = std::time::Instant::max(
            *last_downlink_packet.read().unwrap(),
            *last_uplink_packet.read().unwrap(),
        );
        let bytes_uplink = *bytes_uplink.read().unwrap();
        let bytes_downlink = *bytes_downlink.read().unwrap();
        let packets_uplink = *packets_uplink.read().unwrap();
        let packets_downlink = *packets_downlink.read().unwrap();
        Ok(FlowMetrics {
            flow,
            start,
            end: last_packet,
            bytes_uplink,
            bytes_downlink,
            packets_uplink,
            packets_downlink,
        })
    }
}

#[derive(Debug, Error)]
pub enum ProxyError {
    #[error("Failed to establish proxy chain")]
    EstablishError(#[from] EstablishError),
    #[error("Failed to send to upstream")]
    SendUpstream(#[from] SendError),
    #[error("Failed to recv from upstream")]
    RecvUpstream(#[from] RecvError),
    #[error("Failed to send to downstream")]
    SendDownstream {
        #[source]
        source: io::Error,
        downstream: UdpDownstreamWriter,
    },
}

#[async_trait]
impl UdpServerHook for UdpAccess {
    async fn parse_upstream_addr<'buf>(
        &self,
        buf: &'buf [u8],
        _downstream_writer: &UdpDownstreamWriter,
    ) -> Option<(UpstreamAddr, &'buf [u8])> {
        Some((
            UpstreamAddr(any_addr(&Ipv4Addr::UNSPECIFIED.into()).into()),
            buf,
        ))
    }

    async fn handle_flow(
        &self,
        rx: mpsc::Receiver<Packet>,
        flow: Flow,
        downstream_writer: UdpDownstreamWriter,
    ) {
        let res = self.proxy(rx, flow, downstream_writer).await;
        match res {
            Ok(metrics) => {
                info!(%metrics, "Proxy finished");
            }
            Err(e) => {
                warn!(?e, "Failed to proxy");
            }
        }
    }
}
