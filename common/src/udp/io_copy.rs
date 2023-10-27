use std::{
    io::{self, Write},
    net::SocketAddr,
    sync::{
        atomic::{AtomicU64, AtomicUsize, Ordering},
        Arc, RwLock,
    },
    time::{Duration, SystemTime},
};

use async_speed_limit::Limiter;
use async_trait::async_trait;
use metrics::{counter, decrement_gauge, increment_gauge};
use scopeguard::defer;
use thiserror::Error;
use tokio::{net::UdpSocket, sync::mpsc};
use tracing::{info, trace, warn};

use crate::{
    crypto::{XorCrypto, XorCryptoCursor},
    error::AnyError,
    udp::TIMEOUT,
};

use super::{
    session_table::{Session, UdpSessionTable},
    Flow, FlowMetrics, FlowOwnedGuard, Packet, UdpDownstreamWriter, ACTIVITY_CHECK_INTERVAL,
    BUFFER_LENGTH,
};

const DEAD_SESSION_RETENTION_DURATION: Duration = Duration::from_secs(5);

#[async_trait]
pub trait UdpRecv {
    async fn trait_recv(&mut self, buf: &mut [u8]) -> Result<usize, AnyError>;
}

#[async_trait]
pub trait UdpSend {
    async fn trait_send(&mut self, buf: &[u8]) -> Result<usize, AnyError>;
}

pub struct UpstreamParts<R, W> {
    pub read: R,
    pub write: W,
}

pub struct DownstreamParts {
    pub rx: mpsc::Receiver<Packet>,
    pub write: UdpDownstreamWriter,
}

pub struct CopyBidirectional<R, W> {
    pub flow: FlowOwnedGuard,
    pub upstream: UpstreamParts<R, W>,
    pub downstream: DownstreamParts,
    pub speed_limiter: Limiter,
    pub payload_crypto: Option<XorCrypto>,
    pub response_header: Option<Arc<[u8]>>,
}

impl<R, W> CopyBidirectional<R, W>
where
    R: UdpRecv + Send + 'static,
    W: UdpSend + Send + 'static,
{
    pub async fn serve_as_access_server(
        self,
        session_table: UdpSessionTable,
        upstream_local: Option<SocketAddr>,
        log_prefix: &str,
    ) -> Result<FlowMetrics, CopyBiError> {
        let session_guard = session_table.set_scope_owned(Session {
            start: SystemTime::now(),
            end: None,
            destination: self.flow.flow().upstream.0.clone(),
            upstream_local,
        });

        let res = self.serve_as_proxy_server(log_prefix).await;

        session_guard.inspect_mut(|session| {
            session.end = Some(SystemTime::now());
        });
        tokio::spawn(async move {
            let _session_guard = session_guard;
            tokio::time::sleep(DEAD_SESSION_RETENTION_DURATION).await;
        });

        res
    }

    pub async fn serve_as_proxy_server(self, log_prefix: &str) -> Result<FlowMetrics, CopyBiError> {
        let res = copy_bidirectional(
            self.flow.flow().clone(),
            self.upstream,
            self.downstream,
            self.speed_limiter,
            self.payload_crypto,
            self.response_header,
        )
        .await;

        match &res {
            Ok(metrics) => {
                info!(%metrics, "{log_prefix}: I/O copy finished");
            }
            Err(e) => {
                info!(?e, "{log_prefix}: I/O copy error");
            }
        }

        res
    }
}

pub async fn copy_bidirectional<R, W>(
    flow: Flow,
    mut upstream: UpstreamParts<R, W>,
    mut downstream: DownstreamParts,
    speed_limiter: Limiter,
    payload_crypto: Option<XorCrypto>,
    response_header: Option<Arc<[u8]>>,
) -> Result<FlowMetrics, CopyBiError>
where
    R: UdpRecv + Send + 'static,
    W: UdpSend + Send + 'static,
{
    counter!("udp.io_copies", 1);
    increment_gauge!("udp.current_io_copies", 1.);
    defer!(decrement_gauge!("udp.current_io_copies", 1.));

    let start = std::time::Instant::now();

    // Periodic check if the flow is still alive
    let mut activity_check = tokio::time::interval(ACTIVITY_CHECK_INTERVAL);
    let last_uplink_packet = Arc::new(RwLock::new(std::time::Instant::now()));
    let last_downlink_packet = Arc::new(RwLock::new(std::time::Instant::now()));

    let bytes_uplink = Arc::new(AtomicU64::new(0));
    let bytes_downlink = Arc::new(AtomicU64::new(0));
    let packets_uplink = Arc::new(AtomicUsize::new(0));
    let packets_downlink = Arc::new(AtomicUsize::new(0));

    let mut io_copy_tasks = tokio::task::JoinSet::<Result<(), CopyBiError>>::new();
    io_copy_tasks.spawn({
        let last_uplink_packet = Arc::clone(&last_uplink_packet);
        let bytes_uplink = Arc::clone(&bytes_uplink);
        let packets_uplink = Arc::clone(&packets_uplink);
        let speed_limiter = speed_limiter.clone();
        let payload_crypto = payload_crypto.clone();
        async move {
            loop {
                let res = downstream.rx.recv().await;
                trace!("Received packet from downstream");
                let mut packet = match res {
                    Some(packet) => packet,
                    None => {
                        // Channel closed
                        break;
                    }
                };

                // Limit speed
                speed_limiter.consume(packet.slice().len()).await;

                // Xor payload
                if let Some(payload_crypto) = &payload_crypto {
                    let mut crypto_cursor = XorCryptoCursor::new(payload_crypto);
                    crypto_cursor.xor(packet.slice_mut());
                }

                // Send packet to upstream
                upstream
                    .write
                    .trait_send(packet.slice())
                    .await
                    .map_err(CopyBiError::SendUpstream)?;

                bytes_uplink.fetch_add(packet.slice().len() as u64, Ordering::Relaxed);
                packets_uplink.fetch_add(1, Ordering::Relaxed);
                *last_uplink_packet.write().unwrap() = std::time::Instant::now();
            }
            Ok(())
        }
    });
    io_copy_tasks.spawn({
        let flow = flow.clone();
        let last_downlink_packet = Arc::clone(&last_downlink_packet);
        let bytes_downlink = Arc::clone(&bytes_downlink);
        let packets_downlink = Arc::clone(&packets_downlink);
        let payload_crypto = payload_crypto.clone();
        let mut downlink_buf = [0; BUFFER_LENGTH];
        let mut downlink_protocol_buf = Vec::new();
        async move {
            loop {
                let res = upstream.read.trait_recv(&mut downlink_buf).await;
                trace!("Received packet from upstream");
                let n = res.map_err(CopyBiError::RecvUpstream)?;
                let pkt = &mut downlink_buf[..n];

                // Limit speed
                speed_limiter.consume(pkt.len()).await;

                if n == BUFFER_LENGTH {
                    warn!(
                        ?flow,
                        ?n,
                        "Received downlink packet of size may be too large"
                    );
                    continue;
                }

                // Xor payload
                if let Some(payload_crypto) = &payload_crypto {
                    let mut crypto_cursor = XorCryptoCursor::new(payload_crypto);
                    crypto_cursor.xor(pkt);
                }

                let pkt = if let Some(response_header) = &response_header {
                    // Set up protocol buffer writer
                    downlink_protocol_buf.clear();

                    // Write header
                    downlink_protocol_buf
                        .write_all(response_header.as_ref())
                        .unwrap();

                    // Write payload
                    downlink_protocol_buf.write_all(pkt).unwrap();

                    downlink_protocol_buf.as_slice()
                } else {
                    pkt
                };

                // Send packet to downstream
                downstream
                    .write
                    .send(pkt)
                    .await
                    .map_err(|e| CopyBiError::SendDownstream {
                        source: e,
                        downstream: downstream.write.clone(),
                    })?;

                bytes_downlink.fetch_add(pkt.len() as u64, Ordering::Relaxed);
                packets_downlink.fetch_add(1, Ordering::Relaxed);
                *last_downlink_packet.write().unwrap() = std::time::Instant::now();
            }
        }
    });

    // Forward packets
    loop {
        trace!("Waiting for packet");
        tokio::select! {
            res = io_copy_tasks.join_next() => {
                let res = match res {
                    Some(res) => res.unwrap(),
                    None => break,
                };
                res?;
            }
            _ = activity_check.tick() => {
                trace!("Checking if flow is still alive");
                let now = std::time::Instant::now();
                let last_uplink_packet = *last_uplink_packet.read().unwrap();
                let last_downlink_packet = *last_downlink_packet.read().unwrap();

                if now.duration_since(last_uplink_packet) > TIMEOUT && now.duration_since(last_downlink_packet) > TIMEOUT {
                    trace!(?flow, "Flow timed out");
                    io_copy_tasks.abort_all();
                    break;
                }
            }
        }
    }

    let last_packet = std::time::Instant::max(
        *last_downlink_packet.read().unwrap(),
        *last_uplink_packet.read().unwrap(),
    );
    counter!(
        "udp.io_copy.bytes_uplink",
        bytes_uplink.load(Ordering::Relaxed)
    );
    counter!(
        "udp.io_copy.bytes_downlink",
        bytes_downlink.load(Ordering::Relaxed)
    );
    counter!(
        "udp.io_copy.packets_uplink",
        packets_uplink.load(Ordering::Relaxed) as _
    );
    counter!(
        "udp.io_copy.packets_downlink",
        packets_downlink.load(Ordering::Relaxed) as _
    );
    Ok(FlowMetrics {
        flow,
        start,
        end: last_packet,
        bytes_uplink: bytes_uplink.load(Ordering::Relaxed),
        bytes_downlink: bytes_downlink.load(Ordering::Relaxed),
        packets_uplink: packets_uplink.load(Ordering::Relaxed),
        packets_downlink: packets_downlink.load(Ordering::Relaxed),
    })
}

#[derive(Debug, Error)]
pub enum CopyBiError {
    #[error("Failed to send to upstream: {0}")]
    SendUpstream(#[source] AnyError),
    #[error("Failed to recv from upstream: {0}")]
    RecvUpstream(#[source] AnyError),
    #[error("Failed to send to downstream: {source}, {downstream:?}")]
    SendDownstream {
        #[source]
        source: io::Error,
        downstream: UdpDownstreamWriter,
    },
}

#[async_trait]
impl UdpSend for Arc<UdpSocket> {
    async fn trait_send(&mut self, buf: &[u8]) -> Result<usize, AnyError> {
        UdpSocket::send(self, buf).await.map_err(|e| e.into())
    }
}

#[async_trait]
impl UdpRecv for Arc<UdpSocket> {
    async fn trait_recv(&mut self, buf: &mut [u8]) -> Result<usize, AnyError> {
        UdpSocket::recv(self, buf).await.map_err(|e| e.into())
    }
}
