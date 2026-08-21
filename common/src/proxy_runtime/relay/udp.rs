use crate::{
    addr::InternetAddr,
    anti_replay::VALIDATOR_UDP_HDR_TTL,
    error::AnyError,
    log::Timing,
    proxy_runtime::{
        conn::udp::Flow,
        log::udp::{FlowLog, LOGGER, TrafficLog},
        metrics::udp::{UdpSession, UdpSessionTable},
        relay::{
            EncryptionDirection, decrypt_packet_payload, encrypt_packet_payload,
            retain_dead_session,
        },
    },
    ttl_cell::RegeneratingHeader,
    udp_runtime::{PACKET_BUFFER_LENGTH, Packet, UDP_FLOW_TIMEOUT},
};
use async_speed_limit::Limiter;
use metrics::{counter, gauge};
use scopeguard::defer;
use std::{
    io,
    net::SocketAddr,
    sync::{
        Arc, Mutex, RwLock,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, SystemTime},
};
use thiserror::Error;
use tokio::net::UdpSocket;
use tokio_throughput::{ReadGauge, WriteGauge};
use tracing::{info, trace, warn};
use udp_listener::{ConnRead, ConnWrite};

const ACTIVITY_CHECK_INTERVAL: Duration = Duration::from_secs(1);
const CRYPTO_FAIL_WARN_INTERVAL: Duration = Duration::from_secs(1);

pub trait UdpRecv {
    fn trait_recv(
        &mut self,
        buf: &mut [u8],
    ) -> impl Future<Output = Result<usize, AnyError>> + Send;
}

/// How a write-half shutdown resolved.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShutdownOutcome {
    /// The shutdown signalled a clean EOF to the peer: the flow closed via
    /// the explicit epilog rather than its safety timeout.
    Propagated,
    /// The transport cannot signal EOF (a raw datagram socket): the flow is
    /// retained until its safety timeout.
    Unsupported,
}

pub trait UdpSend {
    fn trait_send(&mut self, buf: &[u8]) -> impl Future<Output = Result<usize, AnyError>> + Send;
    /// Gracefully close the write half, flushing any in-flight frame and
    /// signalling EOF to the peer, reporting whether the close propagated.
    /// Datagram sockets have no half-close, so the default reports
    /// [`ShutdownOutcome::Unsupported`] and the flow is retained until its
    /// safety timeout; mux-stream writers override it to propagate a clean
    /// EOF to the next relay hop and report [`ShutdownOutcome::Propagated`].
    fn trait_shutdown(&mut self) -> impl Future<Output = Result<ShutdownOutcome, AnyError>> + Send {
        async { Ok(ShutdownOutcome::Unsupported) }
    }
}

pub struct UpstreamParts<R, W> {
    pub read: R,
    pub write: W,
}
pub struct DownstreamParts<R, W> {
    pub read: R,
    pub write: W,
}
pub struct CopyBidirectional<R, W, DownstreamRead, DownstreamWrite> {
    pub flow: Flow,
    pub upstream: UpstreamParts<R, W>,
    pub downstream: DownstreamParts<DownstreamRead, DownstreamWrite>,
    pub speed_limiter: Limiter,
    pub payload_crypto: Option<tokio_chacha20::config::Config>,
    pub response_header: Option<Box<dyn Fn() -> Arc<[u8]> + Send>>,
    pub retention: crate::lifecycle::retention::RetentionActorSender,
}

impl<R, W, DownstreamRead, DownstreamWrite> CopyBidirectional<R, W, DownstreamRead, DownstreamWrite>
where
    R: UdpRecv + Send + 'static,
    W: UdpSend + Send + 'static,
    DownstreamRead: UdpRecv + Send + 'static,
    DownstreamWrite: UdpSend + Send + 'static,
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
            let session = UdpSession {
                start: SystemTime::now(),
                end: None,
                destination: None,
                upstream_local,
                upstream_remote: self.flow.upstream.as_ref().unwrap().0.address.clone(),
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
            let session = UdpSession {
                start: SystemTime::now(),
                end: None,
                destination: Some(self.flow.upstream.as_ref().unwrap().0.address.clone()),
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
            monitor_table::table::RowOwnedGuard<UdpSession>,
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
                retain_dead_session(session, &self.retention).await;

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

pub async fn copy_bidirectional<R, W, DownstreamRead, DownstreamWrite>(
    flow: Flow,
    streams: (
        UpstreamParts<R, W>,
        DownstreamParts<DownstreamRead, DownstreamWrite>,
    ),
    speed_limiter: Limiter,
    payload_crypto: Option<tokio_chacha20::config::Config>,
    response_header: Option<Box<dyn Fn() -> Arc<[u8]> + Send>>,
    en_dir: EncryptionDirection,
    gauges: Option<(ReadGauge, WriteGauge)>,
) -> Result<FlowLog, CopyBiError>
where
    R: UdpRecv + Send + 'static,
    W: UdpSend + Send + 'static,
    DownstreamRead: UdpRecv + Send + 'static,
    DownstreamWrite: UdpSend + Send + 'static,
{
    counter!("udp.io_copies").increment(1);
    gauge!("udp.current_io_copies").increment(1.);
    defer!(gauge!("udp.current_io_copies").decrement(1.));
    let start = (std::time::Instant::now(), std::time::SystemTime::now());
    let (mut upstream, mut downstream) = streams;
    let mut activity_check = tokio::time::interval(ACTIVITY_CHECK_INTERVAL);
    let last_uplink_packet = Arc::new(RwLock::new(std::time::Instant::now()));
    let last_downlink_packet = Arc::new(RwLock::new(std::time::Instant::now()));
    let last_crypto_fail_warn = Arc::new(RwLock::new(std::time::Instant::now()));
    let bytes_uplink = Arc::new(AtomicU64::new(0));
    let bytes_downlink = Arc::new(AtomicU64::new(0));
    let packets_uplink = Arc::new(AtomicU64::new(0));
    let packets_downlink = Arc::new(AtomicU64::new(0));
    let mut send_dyn_buf = [0; PACKET_BUFFER_LENGTH];
    let (up_gauge, dn_gauge) = gauges
        .map(|(r, w)| (Some(r.0), Some(w.0)))
        .unwrap_or((None, None));
    let mut io_copy_tasks = tokio::task::JoinSet::<Result<(), CopyBiError>>::new();
    io_copy_tasks.spawn({
        let flow = flow.clone();
        let last_uplink_packet = Arc::clone(&last_uplink_packet);
        let bytes_uplink = Arc::clone(&bytes_uplink);
        let packets_uplink = Arc::clone(&packets_uplink);
        let last_crypto_fail_warn = Arc::clone(&last_crypto_fail_warn);
        let speed_limiter = speed_limiter.clone();
        let payload_crypto = payload_crypto.clone();
        async move {
            let mut downstream_buf = [0; PACKET_BUFFER_LENGTH];
            loop {
                let res = downstream.read.trait_recv(&mut downstream_buf).await;
                trace!("Received packet from downstream");
                let n = match res {
                    Ok(n) => n,
                    Err(error)
                        if error
                            .downcast_ref::<io::Error>()
                            .is_some_and(|error| error.kind() == io::ErrorKind::UnexpectedEof) =>
                    {
                        // The peer closed its write half: shut down the next
                        // hop's writer so the clean EOF propagates down the
                        // chain instead of idling the upstream flow out.
                        let outcome = upstream
                            .write
                            .trait_shutdown()
                            .await
                            .map_err(CopyBiError::SendUpstream)?;
                        if outcome == ShutdownOutcome::Unsupported {
                            // A raw-UDP hop cannot signal EOF: the flow is
                            // retained until the safety timeout below aborts it.
                            trace!(?flow, "Downstream closed but the upstream hop cannot signal EOF; flow retained until its safety timeout");
                        }
                        break;
                    }
                    Err(error) => return Err(CopyBiError::RecvDownstream(error)),
                };
                let packet = &mut downstream_buf[..n];
                speed_limiter.consume(packet.len()).await;
                if let Some(g) = &up_gauge {
                    g.add(packet.len() as u64);
                }
                let packet = if let Some(payload_crypto) = &payload_crypto {
                    let Some(pkt) = send_dyn(packet, &mut send_dyn_buf, payload_crypto, en_dir)
                    else {
                        log_crypto_drop(&flow, &last_crypto_fail_warn);
                        continue;
                    };
                    pkt
                } else {
                    packet
                };
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
        let last_crypto_fail_warn = Arc::clone(&last_crypto_fail_warn);
        let payload_crypto = payload_crypto.clone();
        let mut downlink_buf = [0; PACKET_BUFFER_LENGTH];
        let mut downlink_protocol_buf = vec![];
        let mut response_header_ttl =
            response_header.map(|f| RegeneratingHeader::new(f, VALIDATOR_UDP_HDR_TTL));
        async move {
            loop {
                let res = upstream.read.trait_recv(&mut downlink_buf).await;
                trace!("Received packet from upstream");
                let n = match res {
                    Ok(n) => n,
                    Err(error)
                        if error
                            .downcast_ref::<io::Error>()
                            .is_some_and(|error| error.kind() == io::ErrorKind::UnexpectedEof) =>
                    {
                        // The next hop closed its writer: shut down our
                        // downstream writer so the clean EOF reaches the peer
                        // instead of idling the flow out.
                        let outcome = downstream
                            .write
                            .trait_shutdown()
                            .await
                            .map_err(CopyBiError::SendDownstream)?;
                        if outcome == ShutdownOutcome::Unsupported {
                            // A raw-UDP hop cannot signal EOF: the flow is
                            // retained until the safety timeout below aborts it.
                            trace!(?flow, "Upstream closed but the downstream hop cannot signal EOF; flow retained until its safety timeout");
                        }
                        break;
                    }
                    Err(error) => return Err(CopyBiError::RecvUpstream(error)),
                };
                let pkt = &mut downlink_buf[..n];
                speed_limiter.consume(pkt.len()).await;
                if n == PACKET_BUFFER_LENGTH {
                    warn!(
                        ?flow,
                        ?n,
                        "Received downlink packet of size may be too large"
                    );
                    continue;
                }
                if let Some(g) = &dn_gauge {
                    g.add(pkt.len() as u64);
                }
                let pkt = if let Some(payload_crypto) = &payload_crypto {
                    let Some(pkt) = send_dyn(pkt, &mut send_dyn_buf, payload_crypto, en_dir.flip())
                    else {
                        log_crypto_drop(&flow, &last_crypto_fail_warn);
                        continue;
                    };
                    pkt
                } else {
                    pkt
                };
                let downlink_n = if let Some(response_header) = &mut response_header_ttl {
                    let hdr = response_header.get();
                    downlink_protocol_buf.clear();
                    downlink_protocol_buf.extend_from_slice(hdr);
                    downlink_protocol_buf.extend_from_slice(pkt);
                    downstream.write.trait_send(&downlink_protocol_buf).await
                } else {
                    downstream.write.trait_send(pkt).await
                }
                .map_err(CopyBiError::SendDownstream)?;
                bytes_downlink.fetch_add(downlink_n as u64, Ordering::Relaxed);
                packets_downlink.fetch_add(1, Ordering::Relaxed);
                *last_downlink_packet.write().unwrap() = std::time::Instant::now();
            }
            Ok(())
        }
    });
    let mut outcome: Result<(), CopyBiError> = Ok(());
    loop {
        trace!("Waiting for packet");
        tokio::select! {
            res = io_copy_tasks.join_next() => {
                let Some(res) = res else { break };
                match res.unwrap() {
                    Ok(()) => {}
                    Err(error) => {
                        outcome = Err(error);
                        break;
                    }
                }
            }
            _ = activity_check.tick() => {
                trace!("Checking if flow is still alive");
                let now = std::time::Instant::now();
                let last_uplink_packet = *last_uplink_packet.read().unwrap();
                let last_downlink_packet = *last_downlink_packet.read().unwrap();
                if now.duration_since(last_uplink_packet) > UDP_FLOW_TIMEOUT && now.duration_since(last_downlink_packet) > UDP_FLOW_TIMEOUT {
                    trace!(?flow, "Flow timed out");
                    break;
                }
            }
        }
    }
    // Reap whatever is still in flight (the other direction after the first
    // ended, or both after the timeout): the selected outcome wins, a
    // completed sibling error is folded in only while the outcome is Ok, and
    // a completed sibling panic still cascades.
    crate::lifecycle::task_scope::abort_and_reap_results(&mut io_copy_tasks, outcome).await?;
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
    counter!("udp.relay.up.bytes").increment(up.bytes);
    counter!("udp.relay.up.packets").increment(up.packets);
    counter!("udp.relay.dn.bytes").increment(dn.bytes);
    counter!("udp.relay.dn.packets").increment(dn.packets);
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
    #[error("Failed to recv from downstream: {0}")]
    RecvDownstream(#[source] AnyError),
    #[error("Failed to send to downstream: {0}")]
    SendDownstream(#[source] AnyError),
}

impl UdpRecv for ConnRead<Packet> {
    async fn trait_recv(&mut self, buf: &mut [u8]) -> Result<usize, AnyError> {
        let packet = self
            .read_half()
            .recv()
            .await
            .ok_or_else(|| io::Error::new(io::ErrorKind::UnexpectedEof, "UDP flow closed"))?;
        let copied = packet.slice().len().min(buf.len());
        buf[..copied].copy_from_slice(&packet.slice()[..copied]);
        Ok(copied)
    }
}
impl UdpSend for ConnWrite<UdpSocket> {
    async fn trait_send(&mut self, buf: &[u8]) -> Result<usize, AnyError> {
        self.send(buf).await.map_err(Into::into)
    }
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

fn log_crypto_drop(flow: &Flow, last_warn: &RwLock<std::time::Instant>) {
    counter!("udp.relay.crypto_drops").increment(1);
    let now = std::time::Instant::now();
    let mut last_warn = last_warn.write().unwrap();
    if now.duration_since(*last_warn) > CRYPTO_FAIL_WARN_INTERVAL {
        *last_warn = now;
        warn!(?flow, "Dropped packet due to crypto failure");
    }
}

/// Apply the payload crypto in the given direction, returning the transformed
/// packet or `None` on failure. Named `send_dyn` (rather than the direction
/// word) to dodge the inherent `send` method on the sockets involved.
fn send_dyn<'buf>(
    pkt: &'buf mut [u8],
    buf: &'buf mut [u8],
    config: &tokio_chacha20::config::Config,
    en_dir: EncryptionDirection,
) -> Option<&'buf [u8]> {
    match en_dir {
        EncryptionDirection::Encrypt => encrypt_packet_payload(pkt, buf, config).ok(),
        EncryptionDirection::Decrypt => decrypt_packet_payload(pkt, buf, config).ok(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio_chacha20::X_NONCE_BYTES;
    fn config() -> tokio_chacha20::config::Config {
        tokio_chacha20::config::Config::new([7; tokio_chacha20::KEY_BYTES].into())
    }
    #[test]
    fn a_packet_too_big_to_encrypt_is_dropped_not_spun_on() {
        let config = config();
        let mut buf = vec![0; PACKET_BUFFER_LENGTH];
        for len in [
            PACKET_BUFFER_LENGTH - X_NONCE_BYTES + 1,
            PACKET_BUFFER_LENGTH - 1,
            PACKET_BUFFER_LENGTH,
        ] {
            let mut pkt = vec![0xab; len];
            assert!(
                send_dyn(&mut pkt, &mut buf, &config, EncryptionDirection::Encrypt).is_none(),
                "a {len}-byte packet must be dropped, not encrypted into a {} byte buffer",
                buf.len(),
            );
        }
    }
    #[test]
    fn a_packet_that_fits_round_trips() {
        let config = config();
        let mut en_buf = vec![0; PACKET_BUFFER_LENGTH];
        let mut de_buf = vec![0; PACKET_BUFFER_LENGTH];
        for len in [
            0,
            1,
            23,
            24,
            25,
            1400,
            8192,
            PACKET_BUFFER_LENGTH - X_NONCE_BYTES,
        ] {
            let plain: Vec<u8> = (0..len).map(|i| (i % 251) as u8).collect();
            let mut pkt = plain.clone();
            let ct = send_dyn(&mut pkt, &mut en_buf, &config, EncryptionDirection::Encrypt)
                .unwrap_or_else(|| panic!("a {len}-byte packet fits and must encrypt"))
                .to_vec();
            assert_eq!(ct.len(), len + X_NONCE_BYTES);
            let mut ct = ct;
            let pt = send_dyn(&mut ct, &mut de_buf, &config, EncryptionDirection::Decrypt)
                .unwrap_or_else(|| panic!("a {len}-byte packet must decrypt"));
            assert_eq!(pt, &plain[..], "a {len}-byte packet did not round-trip");
        }
    }
    #[test]
    fn a_packet_shorter_than_the_nonce_is_dropped() {
        let config = config();
        let mut buf = vec![0; PACKET_BUFFER_LENGTH];
        for len in 0..X_NONCE_BYTES {
            let mut pkt = vec![0xab; len];
            assert!(
                send_dyn(&mut pkt, &mut buf, &config, EncryptionDirection::Decrypt).is_none(),
                "a {len}-byte ciphertext is shorter than the {X_NONCE_BYTES}-byte nonce",
            );
        }
    }
}
