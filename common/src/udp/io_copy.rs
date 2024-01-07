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
use metrics::{counter, gauge};
use monitor_table::table::RowOwnedGuard;
use scopeguard::defer;
use thiserror::Error;
use tokio::{net::UdpSocket, sync::mpsc};
use tracing::{info, trace, warn};

use crate::{error::AnyError, udp::TIMEOUT};

use super::{
    session_table::{Session, UdpSessionTable},
    Flow, FlowMetrics, FlowOwnedGuard, Packet, UdpDownstreamWriter, ACTIVITY_CHECK_INTERVAL,
    BUFFER_LENGTH,
};

const DEAD_SESSION_RETENTION_DURATION: Duration = Duration::from_secs(5);

pub trait UdpRecv {
    fn trait_recv(
        &mut self,
        buf: &mut [u8],
    ) -> impl std::future::Future<Output = Result<usize, AnyError>> + Send;
}

pub trait UdpSend {
    fn trait_send(
        &mut self,
        buf: &[u8],
    ) -> impl std::future::Future<Output = Result<usize, AnyError>> + Send;
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
    pub payload_crypto: Option<tokio_chacha20::config::Config>,
    pub response_header: Option<Arc<[u8]>>,
}

impl<R, W> CopyBidirectional<R, W>
where
    R: UdpRecv + Send + 'static,
    W: UdpSend + Send + 'static,
{
    pub async fn serve_as_proxy_server(
        self,
        session_table: Option<UdpSessionTable>,
        upstream_local: Option<SocketAddr>,
        log_prefix: &str,
    ) -> Result<FlowMetrics, CopyBiError> {
        let session = Session {
            start: SystemTime::now(),
            end: None,
            destination: None,
            upstream_local,
            upstream_remote: self.flow.flow().upstream.0.clone(),
            downstream_remote: self.flow.flow().downstream.0,
        };
        let session = session_table.as_ref().map(|s| s.set_scope_owned(session));

        self.serve(session, log_prefix, EncryptionDirection::Decrypt)
            .await
    }

    pub async fn serve_as_access_server(
        self,
        session_table: Option<UdpSessionTable>,
        upstream_local: Option<SocketAddr>,
        log_prefix: &str,
    ) -> Result<FlowMetrics, CopyBiError> {
        let session = Session {
            start: SystemTime::now(),
            end: None,
            destination: Some(self.flow.flow().upstream.0.clone()),
            upstream_local,
            upstream_remote: self.flow.flow().upstream.0.clone(),
            downstream_remote: self.flow.flow().downstream.0,
        };
        let session = session_table.as_ref().map(|s| s.set_scope_owned(session));

        self.serve(session, log_prefix, EncryptionDirection::Encrypt)
            .await
    }

    async fn serve(
        self,
        session: Option<RowOwnedGuard<Session>>,
        log_prefix: &str,
        en_dir: EncryptionDirection,
    ) -> Result<FlowMetrics, CopyBiError> {
        let res = copy_bidirectional(
            self.flow.flow().clone(),
            self.upstream,
            self.downstream,
            self.speed_limiter,
            self.payload_crypto,
            self.response_header,
            en_dir,
        )
        .await;

        if let Some(s) = &session {
            s.inspect_mut(|session| {
                session.end = Some(SystemTime::now());
            });
        }
        tokio::spawn(async move {
            let _session = session;
            tokio::time::sleep(DEAD_SESSION_RETENTION_DURATION).await;
        });

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
    payload_crypto: Option<tokio_chacha20::config::Config>,
    response_header: Option<Arc<[u8]>>,
    en_dir: EncryptionDirection,
) -> Result<FlowMetrics, CopyBiError>
where
    R: UdpRecv + Send + 'static,
    W: UdpSend + Send + 'static,
{
    counter!("udp.io_copies").increment(1);
    gauge!("udp.current_io_copies").increment(1.);
    defer!(gauge!("udp.current_io_copies").decrement(1.));

    let start = std::time::Instant::now();

    // Periodic check if the flow is still alive
    let mut activity_check = tokio::time::interval(ACTIVITY_CHECK_INTERVAL);
    let last_uplink_packet = Arc::new(RwLock::new(std::time::Instant::now()));
    let last_downlink_packet = Arc::new(RwLock::new(std::time::Instant::now()));

    let bytes_uplink = Arc::new(AtomicU64::new(0));
    let bytes_downlink = Arc::new(AtomicU64::new(0));
    let packets_uplink = Arc::new(AtomicUsize::new(0));
    let packets_downlink = Arc::new(AtomicUsize::new(0));

    let mut en_dec_buf = [0; BUFFER_LENGTH];

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

                // Encrypt/Decrypt payload
                let packet = if let Some(payload_crypto) = &payload_crypto {
                    let Some(pkt) =
                        en_dec(packet.slice_mut(), &mut en_dec_buf, payload_crypto, en_dir)
                    else {
                        continue;
                    };
                    pkt
                } else {
                    packet.slice()
                };

                // Send packet to upstream
                upstream
                    .write
                    .trait_send(packet)
                    .await
                    .map_err(CopyBiError::SendUpstream)?;

                bytes_uplink.fetch_add(packet.len() as u64, Ordering::Relaxed);
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

                // Encrypt/Decrypt payload
                let pkt = if let Some(payload_crypto) = &payload_crypto {
                    let Some(pkt) = en_dec(pkt, &mut en_dec_buf, payload_crypto, en_dir.flip())
                    else {
                        continue;
                    };
                    pkt
                } else {
                    pkt
                };

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
    counter!("udp.io_copy.bytes_uplink").increment(bytes_uplink.load(Ordering::Relaxed));
    counter!("udp.io_copy.bytes_downlink").increment(bytes_downlink.load(Ordering::Relaxed));
    counter!("udp.io_copy.packets_uplink").increment(packets_uplink.load(Ordering::Relaxed) as _);
    counter!("udp.io_copy.packets_downlink")
        .increment(packets_downlink.load(Ordering::Relaxed) as _);
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

impl UdpSend for Arc<UdpSocket> {
    async fn trait_send(&mut self, buf: &[u8]) -> Result<usize, AnyError> {
        UdpSocket::send(self, buf).await.map_err(|e| e.into())
    }
}

impl UdpRecv for Arc<UdpSocket> {
    async fn trait_recv(&mut self, buf: &mut [u8]) -> Result<usize, AnyError> {
        UdpSocket::recv(self, buf).await.map_err(|e| e.into())
    }
}

#[derive(Debug, Clone, Copy)]
pub enum EncryptionDirection {
    Encrypt,
    Decrypt,
}
impl EncryptionDirection {
    pub fn flip(&self) -> Self {
        match self {
            Self::Encrypt => Self::Decrypt,
            Self::Decrypt => Self::Encrypt,
        }
    }
}

fn en_dec<'buf>(
    pkt: &'buf mut [u8],
    buf: &'buf mut [u8],
    config: &tokio_chacha20::config::Config,
    en_dir: EncryptionDirection,
) -> Option<&'buf [u8]> {
    Some(match en_dir {
        EncryptionDirection::Encrypt => {
            let mut cursor = tokio_chacha20::cursor::EncryptCursor::new(*config.key());

            let (f, t) = cursor.encrypt(pkt, buf);
            if pkt.len() != f {
                // Not fully copied.
                return None;
            }
            &buf[..t]
        }
        EncryptionDirection::Decrypt => {
            let mut cursor = tokio_chacha20::cursor::DecryptCursor::new(*config.key());
            let i = cursor.decrypt(pkt).unwrap();
            &pkt[i..]
        }
    })
}
