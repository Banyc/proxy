use std::{
    io::{self, Write},
    net::SocketAddr,
    pin::Pin,
    sync::{
        Arc, Mutex, RwLock,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, SystemTime},
};

use async_speed_limit::Limiter;
use metrics::{counter, gauge};
use scopeguard::defer;
use thiserror::Error;
use tokio::{
    io::{AsyncRead, AsyncWrite, ReadBuf},
    net::UdpSocket,
};
use tokio_throughput::{ReadGauge, WriteGauge};
use tracing::{info, trace, warn};
use udp_listener::{ConnRead, ConnWrite};

use crate::{
    addr::InternetAddr,
    anti_replay::VALIDATOR_UDP_HDR_TTL,
    error::AnyError,
    log::Timing,
    proto::{
        conn::udp::Flow,
        io_copy::{nonce_ciphertext_reader, nonce_ciphertext_writer, noop_context, unwrap_ready},
        log::udp::{FlowLog, LOGGER, TrafficLog},
        metrics::udp::{Session, UdpSessionTable},
    },
    ttl_cell::TtlCell,
    udp::{PACKET_BUFFER_LENGTH, Packet, TIMEOUT},
};

const ACTIVITY_CHECK_INTERVAL: Duration = Duration::from_secs(1);
const DEAD_SESSION_RETENTION_DURATION: Duration = Duration::from_secs(5);

pub trait UdpRecv {
    fn trait_recv(
        &mut self,
        buf: &mut [u8],
    ) -> impl Future<Output = Result<usize, AnyError>> + Send;
}

pub trait UdpSend {
    fn trait_send(&mut self, buf: &[u8]) -> impl Future<Output = Result<usize, AnyError>> + Send;
}

pub struct UpstreamParts<R, W> {
    pub read: R,
    pub write: W,
}

pub struct DownstreamParts {
    pub read: ConnRead<Packet>,
    pub write: ConnWrite<UdpSocket>,
}

pub struct CopyBidirectional<R, W> {
    pub flow: Flow,
    pub upstream: UpstreamParts<R, W>,
    pub downstream: DownstreamParts,
    pub speed_limiter: Limiter,
    pub payload_crypto: Option<tokio_chacha20::config::Config>,
    pub response_header: Option<Box<dyn Fn() -> Arc<[u8]> + Send>>,
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
    ) -> Result<FlowLog, CopyBiError> {
        let session = session_table.as_ref().map(|s| {
            let (up_handle, up) = tokio_throughput::gauge();
            let (dn_handle, dn) = tokio_throughput::gauge();
            let r = ReadGauge(up);
            let w = WriteGauge(dn);

            let session = Session {
                start: SystemTime::now(),
                end: None,
                destination: None,
                upstream_local,
                upstream_remote: self.flow.upstream.as_ref().unwrap().0.clone(),
                downstream_remote: self.flow.downstream.0,
                up_gauge: Mutex::new(up_handle),
                dn_gauge: Mutex::new(dn_handle),
            };
            let session = s.set_scope_owned(session);
            (session, r, w)
        });

        self.serve(session, log_prefix, EncryptionDirection::Decrypt)
            .await
    }

    pub async fn serve_as_access_server(
        self,
        session_table: Option<UdpSessionTable>,
        upstream_local: Option<SocketAddr>,
        upstream_remote: InternetAddr,
        log_prefix: &str,
    ) -> Result<FlowLog, CopyBiError> {
        let session = session_table.as_ref().map(|s| {
            let (up_handle, up) = tokio_throughput::gauge();
            let (dn_handle, dn) = tokio_throughput::gauge();
            let r = ReadGauge(up);
            let w = WriteGauge(dn);

            let session = Session {
                start: SystemTime::now(),
                end: None,
                destination: Some(self.flow.upstream.as_ref().unwrap().0.clone()),
                upstream_local,
                upstream_remote,
                downstream_remote: self.flow.downstream.0,
                up_gauge: Mutex::new(up_handle),
                dn_gauge: Mutex::new(dn_handle),
            };
            let session = s.set_scope_owned(session);
            (session, r, w)
        });

        self.serve(session, log_prefix, EncryptionDirection::Encrypt)
            .await
    }

    async fn serve(
        self,
        session: Option<(
            monitor_table::table::RowOwnedGuard<Session>,
            ReadGauge,
            WriteGauge,
        )>,
        log_prefix: &str,
        en_dir: EncryptionDirection,
    ) -> Result<FlowLog, CopyBiError> {
        let res = match session {
            Some((session, r, w)) => {
                let res = copy_bidirectional(
                    self.flow.clone(),
                    (self.upstream, self.downstream),
                    self.speed_limiter,
                    self.payload_crypto,
                    self.response_header,
                    en_dir,
                    Some((r, w)),
                )
                .await;

                session.inspect_mut(|session| {
                    session.end = Some(SystemTime::now());
                });
                tokio::spawn(async move {
                    let _session = session;
                    tokio::time::sleep(DEAD_SESSION_RETENTION_DURATION).await;
                });

                res
            }
            None => {
                copy_bidirectional(
                    self.flow.clone(),
                    (self.upstream, self.downstream),
                    self.speed_limiter,
                    self.payload_crypto,
                    self.response_header,
                    en_dir,
                    None,
                )
                .await
            }
        };

        match &res {
            Ok(log) => {
                let record = log.into();
                if let Some(x) = LOGGER.lock().unwrap().as_mut() {
                    x.write(&record)
                }

                info!(%log, "{log_prefix}: I/O copy finished");
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
    streams: (UpstreamParts<R, W>, DownstreamParts),
    speed_limiter: Limiter,
    payload_crypto: Option<tokio_chacha20::config::Config>,
    response_header: Option<Box<dyn Fn() -> Arc<[u8]> + Send>>,
    en_dir: EncryptionDirection,
    gauges: Option<(ReadGauge, WriteGauge)>,
) -> Result<FlowLog, CopyBiError>
where
    R: UdpRecv + Send + 'static,
    W: UdpSend + Send + 'static,
{
    counter!("udp.io_copies").increment(1);
    gauge!("udp.current_io_copies").increment(1.);
    defer!(gauge!("udp.current_io_copies").decrement(1.));

    let start = (std::time::Instant::now(), std::time::SystemTime::now());

    let (mut upstream, mut downstream) = streams;

    // Periodic check if the flow is still alive
    let mut activity_check = tokio::time::interval(ACTIVITY_CHECK_INTERVAL);
    let last_uplink_packet = Arc::new(RwLock::new(std::time::Instant::now()));
    let last_downlink_packet = Arc::new(RwLock::new(std::time::Instant::now()));

    let bytes_uplink = Arc::new(AtomicU64::new(0));
    let bytes_downlink = Arc::new(AtomicU64::new(0));
    let packets_uplink = Arc::new(AtomicU64::new(0));
    let packets_downlink = Arc::new(AtomicU64::new(0));

    let mut en_dec_buf = [0; PACKET_BUFFER_LENGTH];

    let (up_gauge, dn_gauge) = gauges
        .map(|(r, w)| (Some(r.0), Some(w.0)))
        .unwrap_or((None, None));

    let mut io_copy_tasks = tokio::task::JoinSet::<Result<(), CopyBiError>>::new();
    io_copy_tasks.spawn({
        let last_uplink_packet = Arc::clone(&last_uplink_packet);
        let bytes_uplink = Arc::clone(&bytes_uplink);
        let packets_uplink = Arc::clone(&packets_uplink);
        let speed_limiter = speed_limiter.clone();
        let payload_crypto = payload_crypto.clone();
        async move {
            loop {
                let res = downstream.read.recv().recv().await;
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

                // Gauge
                if let Some(g) = &up_gauge {
                    g.update(packet.slice().len() as u64);
                }

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
        let mut downlink_buf = [0; PACKET_BUFFER_LENGTH];
        let mut downlink_protocol_buf = Vec::new();
        let mut response_header_ttl =
            TtlCell::new(response_header.as_ref().map(|f| f()), VALIDATOR_UDP_HDR_TTL);
        async move {
            loop {
                let res = upstream.read.trait_recv(&mut downlink_buf).await;
                trace!("Received packet from upstream");
                let n = res.map_err(CopyBiError::RecvUpstream)?;
                let pkt = &mut downlink_buf[..n];

                // Limit speed
                speed_limiter.consume(pkt.len()).await;

                if n == PACKET_BUFFER_LENGTH {
                    warn!(
                        ?flow,
                        ?n,
                        "Received downlink packet of size may be too large"
                    );
                    continue;
                }

                // Gauge
                if let Some(g) = &dn_gauge {
                    g.update(pkt.len() as u64);
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

                    let hdr = match response_header_ttl.get() {
                        Some(hdr) => hdr,
                        None => response_header_ttl.set(response_header()),
                    };

                    // Write header
                    downlink_protocol_buf.write_all(hdr.as_ref()).unwrap();

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
    let up = TrafficLog {
        bytes: bytes_uplink.load(Ordering::Relaxed),
        packets: packets_uplink.load(Ordering::Relaxed),
    };
    let dn = TrafficLog {
        bytes: bytes_downlink.load(Ordering::Relaxed),
        packets: packets_downlink.load(Ordering::Relaxed),
    };
    let timing = Timing {
        start,
        end: last_packet,
    };
    counter!("udp.io_copy.up.bytes").increment(up.bytes);
    counter!("udp.io_copy.up.packets").increment(up.packets);
    counter!("udp.io_copy.dn.bytes").increment(dn.bytes);
    counter!("udp.io_copy.dn.packets").increment(dn.packets);
    Ok(FlowLog {
        flow,
        timing,
        up,
        dn,
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
        downstream: ConnWrite<UdpSocket>,
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
            let mut buf_wtr = io::Cursor::new(&mut *buf);
            let mut w = nonce_ciphertext_writer(config.key(), &mut buf_wtr);
            unwrap_ready(Pin::new(&mut w).poll_write(&mut noop_context(), pkt)).ok()?;
            let pos = usize::try_from(buf_wtr.position()).unwrap();
            &buf[..pos]
        }
        EncryptionDirection::Decrypt => {
            let mut r = nonce_ciphertext_reader(config.key(), &*pkt);
            let mut read_buf = ReadBuf::new(buf);
            unwrap_ready(Pin::new(&mut r).poll_read(&mut noop_context(), &mut read_buf)).ok()?;
            let pos = read_buf.filled().len();
            &buf[..pos]
        }
    })
}
