use crate::{
    addr::InternetAddr,
    anti_replay::{VALIDATOR_TIME_FRAME, VALIDATOR_UDP_HDR_TTL},
    error::AnyError,
    header::{
        codec::{CodecError, read_header, write_header},
        route::{RouteError, RouteRequest, RouteResponse},
    },
    proxy_runtime::{
        addr::RouteAddr,
        conn::udp::UdpFlowId,
        connect::udp::{UdpConnectionRead, UdpConnectionWrite},
        context::UdpRuntime,
        relay::{
            decrypt_packet_payload, encrypt_packet_payload,
            udp::{ShutdownOutcome, UdpRecv, UdpSend},
        },
    },
    route::{ProbeRtt, RouteChain, convert_proxies_to_header_crypto_pairs},
    ttl_cell::RegeneratingHeader,
    udp_runtime::{PACKET_BUFFER_LENGTH, UDP_FLOW_TIMEOUT},
};
use ae::anti_replay::{TimeValidator, ValidatorRef};
use bytes::BytesMut;
use metrics::counter;
use primitive::arena::obj_pool::ArcObjPool;
use std::{
    io::{self, Write},
    net::SocketAddr,
    num::NonZeroUsize,
    sync::{Arc, Mutex},
    time::{Duration, Instant, SystemTime},
};
use thiserror::Error;
use tokio::sync::watch;
use tracing::{instrument, trace, warn};

#[derive(Debug, Default)]
struct RouteConfirmation {
    last_response: Mutex<Option<ConfirmationTime>>,
}
#[derive(Debug)]
struct ConfirmationTime {
    monotonic: Instant,
    wall: SystemTime,
}
impl RouteConfirmation {
    fn confirm(&self) {
        *self.last_response.lock().unwrap() = Some(ConfirmationTime {
            monotonic: Instant::now(),
            wall: SystemTime::now(),
        });
    }
    fn is_fresh(&self) -> bool {
        self.last_response
            .lock()
            .unwrap()
            .as_ref()
            .is_some_and(|confirmed| {
                confirmed.monotonic.elapsed() < UDP_FLOW_TIMEOUT
                    && confirmed
                        .wall
                        .elapsed()
                        .is_ok_and(|elapsed| elapsed < UDP_FLOW_TIMEOUT)
            })
    }
}

#[derive(Debug)]
pub struct UdpProxyClient {
    write: UdpProxyClientWriteHalf,
    read: UdpProxyClientReadHalf,
    upstream_addr: InternetAddr,
}
impl UdpProxyClient {
    #[instrument(skip_all)]
    pub async fn establish(
        proxies: Arc<RouteChain>,
        destination: InternetAddr,
        context: &UdpRuntime,
    ) -> Result<UdpProxyClient, EstablishError> {
        let route_confirmation = Arc::new(RouteConfirmation::default());
        let flow_id = UdpFlowId::random();
        if proxies.is_empty() {
            let addr = *destination
                .to_socket_addrs()
                .await
                .map_err(|e| EstablishError::ResolveDestination {
                    source: e,
                    addr: destination.clone(),
                })?
                .first();
            let upstream = context.connector.connect(addr).await.map_err(|e| {
                EstablishError::ConnectDestination {
                    source: e,
                    addr: destination.clone(),
                    sock_addr: addr,
                }
            })?;
            let upstream = crate::proxy_runtime::connect::udp::UdpConnection::socket(upstream);
            let local_addr = upstream.local_addr();
            let peer_addr = upstream.peer_addr();
            let (upstream_read, upstream_write) = upstream.into_split();
            let write = UdpProxyClientWriteHalf::new(
                upstream_write,
                peer_addr,
                Vec::new(),
                flow_id,
                route_confirmation.clone(),
            );
            let read = UdpProxyClientReadHalf::new(
                upstream_read,
                local_addr,
                peer_addr,
                proxies,
                route_confirmation,
            );
            return Ok(UdpProxyClient {
                write,
                read,
                upstream_addr: destination,
            });
        }
        let proxy_addr = proxies[0].address.clone();
        let upstream = context
            .connector
            .connect_route(&proxy_addr, crate::STREAM_IO_TIMEOUT)
            .await
            .map_err(|source| EstablishError::ConnectFirstProxy {
                source,
                addr: proxy_addr.clone(),
            })?;
        let pairs =
            convert_proxies_to_header_crypto_pairs(&proxies, Some(RouteAddr::udp(destination)));
        let pairs = pairs
            .into_iter()
            .collect::<Vec<(RouteRequest<RouteAddr>, &tokio_chacha20::config::Config)>>();
        let request_layers = pairs
            .into_iter()
            .zip(proxies.iter())
            .map(|((header, header_crypto), proxy)| {
                let header_crypto = header_crypto.clone();
                let regenerate = Box::new(move || {
                    let mut buf = Vec::new();
                    trace!(?header, "Writing header to buffer");
                    write_header(&mut buf, &header, *header_crypto.key()).unwrap();
                    buf.into()
                });
                UdpRequestLayer {
                    header: RegeneratingHeader::new(regenerate, VALIDATOR_UDP_HDR_TTL),
                    payload_crypto: proxy.payload_crypto.clone(),
                }
            })
            .collect();
        let local_addr = upstream.local_addr();
        let peer_addr = upstream.peer_addr();
        let (upstream_read, upstream_write) = upstream.into_split();
        let write = UdpProxyClientWriteHalf::new(
            upstream_write,
            peer_addr,
            request_layers,
            flow_id,
            route_confirmation.clone(),
        );
        let read = UdpProxyClientReadHalf::new(
            upstream_read,
            local_addr,
            peer_addr,
            proxies,
            route_confirmation,
        );
        Ok(UdpProxyClient {
            write,
            read,
            upstream_addr: proxy_addr.address,
        })
    }

    pub fn into_split(self) -> (UdpProxyClientReadHalf, UdpProxyClientWriteHalf) {
        (self.read, self.write)
    }

    pub fn remote_addr(&self) -> &InternetAddr {
        &self.upstream_addr
    }
}
#[derive(Debug, Error)]
pub enum EstablishError {
    #[error("Failed to resolve destination address: {source}, {addr}")]
    ResolveDestination {
        #[source]
        source: io::Error,
        addr: InternetAddr,
    },
    #[error("Failed to connect to destination: {source}, {addr}, {sock_addr}")]
    ConnectDestination {
        #[source]
        source: io::Error,
        addr: InternetAddr,
        sock_addr: SocketAddr,
    },
    #[error("Failed to connect to first proxy: {source}, {addr}")]
    ConnectFirstProxy {
        #[source]
        source: io::Error,
        addr: RouteAddr,
    },
}

struct UdpRequestLayer {
    header: RegeneratingHeader,
    payload_crypto: Option<tokio_chacha20::config::Config>,
}

pub struct UdpProxyClientWriteHalf {
    upstream: UdpConnectionWrite,
    peer_addr: Option<SocketAddr>,
    request_layers: Vec<UdpRequestLayer>,
    flow_id: UdpFlowId,
    route_confirmation: Arc<RouteConfirmation>,
    write_buf: Vec<u8>,
    transform_buf: Vec<u8>,
}
impl core::fmt::Debug for UdpProxyClientWriteHalf {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("UdpProxyClientWriteHalf")
            .field("upstream", &self.upstream)
            .field("write_buf", &self.write_buf)
            .finish()
    }
}
impl UdpSend for UdpProxyClientWriteHalf {
    async fn trait_send(&mut self, buf: &[u8]) -> Result<usize, AnyError> {
        Self::send(self, buf).await.map_err(|e| e.into())
    }
    async fn trait_shutdown(&mut self) -> Result<ShutdownOutcome, AnyError> {
        self.upstream.trait_shutdown().await
    }
}
impl UdpProxyClientWriteHalf {
    fn new(
        upstream: UdpConnectionWrite,
        peer_addr: Option<SocketAddr>,
        request_layers: Vec<UdpRequestLayer>,
        flow_id: UdpFlowId,
        route_confirmation: Arc<RouteConfirmation>,
    ) -> Self {
        Self {
            upstream,
            peer_addr,
            request_layers,
            flow_id,
            route_confirmation,
            write_buf: Vec::with_capacity(PACKET_BUFFER_LENGTH),
            transform_buf: Vec::with_capacity(PACKET_BUFFER_LENGTH),
        }
    }

    #[instrument(skip_all)]
    pub async fn send(&mut self, buf: &[u8]) -> Result<usize, SendError> {
        self.write_buf.clear();
        self.write_buf.write_all(buf).unwrap();
        let use_compact = self.route_confirmation.is_fresh();
        for layer in self.request_layers.iter_mut().rev() {
            if let Some(payload_crypto) = &layer.payload_crypto {
                self.transform_buf.resize(PACKET_BUFFER_LENGTH, 0);
                let encrypted = encrypt_packet_payload(
                    &self.write_buf,
                    &mut self.transform_buf,
                    payload_crypto,
                )
                .map_err(|source| SendError {
                    source,
                    sock_addr: self.peer_addr,
                })?;
                let encrypted_len = encrypted.len();
                self.transform_buf.truncate(encrypted_len);
                std::mem::swap(&mut self.write_buf, &mut self.transform_buf);
            }
            self.transform_buf.clear();
            if use_compact {
                self.flow_id.write_compact(&mut self.transform_buf);
            } else {
                self.flow_id.write_routed(&mut self.transform_buf);
                self.transform_buf.extend_from_slice(layer.header.get());
            }
            let encoded_len = self
                .transform_buf
                .len()
                .checked_add(self.write_buf.len())
                .ok_or_else(|| SendError {
                    source: io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "UDP packet length overflow",
                    ),
                    sock_addr: self.peer_addr,
                })?;
            if encoded_len > PACKET_BUFFER_LENGTH {
                return Err(SendError {
                    source: io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "encoded UDP proxy packet is too large",
                    ),
                    sock_addr: self.peer_addr,
                });
            }
            self.transform_buf.extend_from_slice(&self.write_buf);
            std::mem::swap(&mut self.write_buf, &mut self.transform_buf);
        }
        self.upstream
            .trait_send(&self.write_buf)
            .await
            .map_err(|e| {
                let peer_addr = self.peer_addr;
                SendError {
                    source: io::Error::other(e),
                    sock_addr: peer_addr,
                }
            })?;
        Ok(buf.len())
    }
}
#[derive(Debug, Error)]
#[error("Failed to send to upstream: {source}, {sock_addr:?}")]
pub struct SendError {
    #[source]
    source: io::Error,
    sock_addr: Option<SocketAddr>,
}

fn time_validator() -> TimeValidator {
    TimeValidator::new(VALIDATOR_TIME_FRAME + VALIDATOR_UDP_HDR_TTL)
}

#[derive(Debug)]
pub struct UdpProxyClientReadHalf {
    upstream: UdpConnectionRead,
    local_addr: Option<SocketAddr>,
    peer_addr: Option<SocketAddr>,
    proxies: Arc<RouteChain>,
    read_buf: Vec<u8>,
    transform_buf: Vec<u8>,
    time_validator: TimeValidator,
    route_confirmation: Arc<RouteConfirmation>,
}
impl UdpRecv for UdpProxyClientReadHalf {
    async fn trait_recv(&mut self, buf: &mut [u8]) -> Result<usize, AnyError> {
        Self::recv(self, buf).await.map_err(|e| e.into())
    }
}
impl UdpProxyClientReadHalf {
    fn new(
        upstream: UdpConnectionRead,
        local_addr: Option<SocketAddr>,
        peer_addr: Option<SocketAddr>,
        proxies: Arc<RouteChain>,
        route_confirmation: Arc<RouteConfirmation>,
    ) -> Self {
        Self {
            upstream,
            local_addr,
            peer_addr,
            proxies,
            read_buf: Vec::with_capacity(PACKET_BUFFER_LENGTH),
            transform_buf: Vec::with_capacity(PACKET_BUFFER_LENGTH),
            time_validator: time_validator(),
            route_confirmation,
        }
    }

    #[instrument(skip_all)]
    pub async fn recv(&mut self, buf: &mut [u8]) -> Result<usize, RecvError> {
        self.read_buf.resize(PACKET_BUFFER_LENGTH, 0);
        let n = self
            .upstream
            .trait_recv(&mut self.read_buf)
            .await
            .map_err(|e| {
                let peer_addr = self.peer_addr;
                RecvError::RecvUpstream {
                    source: io::Error::other(e),
                    sock_addr: peer_addr,
                }
            })?;
        let mut in_read_buf = true;
        let mut start = 0;
        let mut end = n;
        for node in self.proxies.iter() {
            trace!(?node.address, "Reading response");
            let validator = ValidatorRef::Time(&self.time_validator);
            let (resp, consumed): (RouteResponse, usize) = {
                let packet = if in_read_buf {
                    &self.read_buf[start..end]
                } else {
                    &self.transform_buf[start..end]
                };
                let mut reader = io::Cursor::new(packet);
                let response = read_header(&mut reader, *node.header_crypto.key(), &validator)?;
                (response, usize::try_from(reader.position()).unwrap())
            };
            if let Err(err) = resp.result {
                warn!(?err, %node.address, "Upstream responded with an error");
                return Err(RecvError::Response {
                    err,
                    addr: node.address.address.clone(),
                });
            }
            start += consumed;
            if let Some(payload_crypto) = &node.payload_crypto {
                if in_read_buf {
                    self.transform_buf.resize(PACKET_BUFFER_LENGTH, 0);
                    let decrypted = decrypt_packet_payload(
                        &self.read_buf[start..end],
                        &mut self.transform_buf,
                        payload_crypto,
                    )
                    .map_err(|source| RecvError::PayloadCrypto {
                        source,
                        addr: node.address.address.clone(),
                    })?;
                    end = decrypted.len();
                } else {
                    self.read_buf.resize(PACKET_BUFFER_LENGTH, 0);
                    let decrypted = decrypt_packet_payload(
                        &self.transform_buf[start..end],
                        &mut self.read_buf,
                        payload_crypto,
                    )
                    .map_err(|source| RecvError::PayloadCrypto {
                        source,
                        addr: node.address.address.clone(),
                    })?;
                    end = decrypted.len();
                }
                in_read_buf = !in_read_buf;
                start = 0;
            }
        }
        let payload = if in_read_buf {
            &self.read_buf[start..end]
        } else {
            &self.transform_buf[start..end]
        };
        let payload_size = payload.len().min(buf.len());
        buf[..payload_size].copy_from_slice(&payload[..payload_size]);
        if !self.proxies.is_empty() {
            self.route_confirmation.confirm();
        }
        Ok(payload_size)
    }

    pub fn local_addr(&self) -> Option<SocketAddr> {
        self.local_addr
    }
}
#[derive(Debug, Error)]
pub enum RecvError {
    #[error("Failed to recv from upstream: {source}, {sock_addr:?}")]
    RecvUpstream {
        #[source]
        source: io::Error,
        sock_addr: Option<SocketAddr>,
    },
    #[error("Failed to read response from upstream: {0}")]
    Header(#[from] CodecError),
    #[error("Failed to decrypt UDP payload from {addr}: {source}")]
    PayloadCrypto {
        #[source]
        source: io::Error,
        addr: InternetAddr,
    },
    #[error("Upstream responded with an error: {err}, {addr}")]
    Response { err: RouteError, addr: InternetAddr },
}

#[derive(Debug)]
pub struct UdpTracer {
    pool: Arc<ArcObjPool<BytesMut>>,
    context: UdpRuntime,
}
impl UdpTracer {
    pub fn new(context: UdpRuntime) -> Self {
        let pool = ArcObjPool::new(
            None,
            NonZeroUsize::new(1).unwrap(),
            || BytesMut::with_capacity(PACKET_BUFFER_LENGTH),
            |buf| buf.clear(),
        );
        Self {
            pool: Arc::new(pool),
            context,
        }
    }
}
impl ProbeRtt for UdpTracer {
    fn probe_kind(&self) -> &'static str {
        "udp"
    }
    fn probe_rtt(
        &self,
        chain: &RouteChain,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = crate::route::ProbeOutcome> + Send>>
    {
        let pool = self.pool.clone();
        let context = self.context.clone();
        let chain: Vec<crate::route::HopConfig> = chain.to_vec();
        Box::pin(async move {
            let mut pkt_buf = pool.take();
            let outcome = match probe_rtt(&mut pkt_buf, &chain, &context).await {
                Ok((rtt, epilog)) => crate::route::ProbeOutcome {
                    rtt: Ok(rtt),
                    // The teardown epilog is handed to the caller so it is
                    // owned and reaped by `probe_task`'s scoped JoinSet
                    // instead of escaping as a detached task.
                    epilog: epilog.fut,
                },
                Err(error) => crate::route::ProbeOutcome {
                    rtt: Err(error.into()),
                    epilog: None,
                },
            };
            pool.put(pkt_buf);
            outcome
        })
    }
}
/// The probe flow's end state, reported through the completion signal
/// returned with [`probe_rtt`]. The flow has actually terminated once the
/// receiver leaves the pending state. [`ProbeFlowEnd::Eof`] and
/// [`ProbeFlowEnd::TimedOut`] are only reported by the scoped epilog task
/// after it observed the teardown; the other terminal states are facts
/// about the probe itself, reported without any observation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProbeFlowEnd {
    /// The flow is still being torn down; the receiver changes once it
    /// terminates.
    Pending,
    /// No flow was ever opened (an empty chain): there is nothing to tear
    /// down, so no teardown was observed.
    NoFlow,
    /// The write-half shutdown reported [`ShutdownOutcome::Unsupported`]
    /// (a raw-UDP hop): no EOF can arrive on the probe's read half and the
    /// flow is retained until its safety timeout, so there is nothing for
    /// an epilog to observe.
    ShutdownUnsupported,
    /// The scoped epilog task observed a clean EOF at the datagram
    /// boundary: the peer closed the flow in response to the probe's
    /// write-half shutdown, so the epilog propagated end to end.
    Eof,
    /// The scoped epilog task waited out [`FLOW_END_TIMEOUT`] without
    /// observing the flow's end: no EOF arrived, so the flow was retained
    /// until its safety timeout.
    TimedOut,
}

/// How long the probe's teardown epilog waits for the flow's EOF before
/// reporting [`ProbeFlowEnd::TimedOut`]: the flow's safety timeout (which
/// aborts a retained relay after roughly 10s of inactivity) plus margin.
const FLOW_END_TIMEOUT: Duration = Duration::from_secs(15);

/// Read until the peer's EOF, returning `true` on a clean end at a
/// datagram boundary. Datagrams arriving before the close are drained.
async fn await_probe_flow_end(read: &mut UdpConnectionRead) -> bool {
    let mut buf = [0; PACKET_BUFFER_LENGTH];
    loop {
        match read.trait_recv(&mut buf).await {
            Ok(0) => return true,
            Ok(_) => continue,
            Err(error) => {
                return error
                    .downcast_ref::<io::Error>()
                    .is_some_and(|error| error.kind() == io::ErrorKind::UnexpectedEof);
            }
        }
    }
}

/// The probe flow's teardown epilog: once the response is in hand the
/// probe shuts down its write half, and the epilog future observes the
/// flow's actual end — a clean EOF or a wait-out — and reports it through
/// the completion signal. It is returned unspawned so the caller owns it:
/// `probe_task` spawns it into its function-scoped `JoinSet`, which
/// reaps it (aborting outstanding epilogs when the task ends) and re-raises
/// panics on join, instead of escaping as a detached `tokio::spawn` that
/// nobody joins.
pub struct ProbeEpilog {
    /// Completion signal. Tests await it to observe flow termination
    /// deterministically; production probe loops ignore it.
    pub end: watch::Receiver<ProbeFlowEnd>,
    /// The teardown observation future, present only when the write-half
    /// shutdown propagated and there is an end to observe. `None` means
    /// the terminal state was already reported ([`ProbeFlowEnd::NoFlow`]
    /// or [`ProbeFlowEnd::ShutdownUnsupported`]) and there is nothing to
    /// run.
    pub fut: Option<std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>>>,
}

/// Send one echo probe over the proxy chain and read its response.
///
/// Returns the round-trip time together with the probe's teardown epilog:
/// after the response is in hand the probe shuts down its write half (the
/// explicit epilog), and the epilog future observes how the flow actually
/// terminated — a clean EOF when the shutdown propagated, or a wait-out
/// when the peer never closed. The caller must spawn the epilog future
/// (into `probe_task`'s scoped `JoinSet`) so it does not escape scoped
/// ownership.
pub async fn probe_rtt(
    pkt_buf: &mut BytesMut,
    proxies: &RouteChain,
    context: &UdpRuntime,
) -> Result<(Duration, ProbeEpilog), TraceError> {
    if proxies.is_empty() {
        // No flow was ever opened; report the no-flow end immediately so
        // an awaited completion never hangs. There is no teardown to
        // observe, so no epilog future.
        let (end_tx, end_rx) = watch::channel(ProbeFlowEnd::Pending);
        let _ = end_tx.send_replace(ProbeFlowEnd::NoFlow);
        return Ok((
            Duration::from_secs(0),
            ProbeEpilog {
                end: end_rx,
                fut: None,
            },
        ));
    }
    let proxy_addr = &proxies[0].address;
    let upstream = context
        .connector
        .connect_route(proxy_addr, crate::STREAM_IO_TIMEOUT)
        .await?;
    let (mut upstream_read, mut upstream_write) = upstream.into_split();
    let pairs = convert_proxies_to_header_crypto_pairs(proxies, None);
    let mut packet = Vec::new();
    let mut transform_buf = vec![0; PACKET_BUFFER_LENGTH];
    for (index, ((header, header_crypto), proxy)) in pairs.iter().zip(proxies).enumerate().rev() {
        if index + 1 < proxies.len()
            && let Some(payload_crypto) = &proxy.payload_crypto
        {
            transform_buf.resize(PACKET_BUFFER_LENGTH, 0);
            let encrypted = encrypt_packet_payload(&packet, &mut transform_buf, payload_crypto)?;
            let encrypted_len = encrypted.len();
            transform_buf.truncate(encrypted_len);
            std::mem::swap(&mut packet, &mut transform_buf);
            transform_buf.resize(PACKET_BUFFER_LENGTH, 0);
        }
        let mut header_buf = Vec::new();
        UdpFlowId::random().write_routed(&mut header_buf);
        write_header(&mut header_buf, header, *header_crypto.key())?;
        let encoded_len = header_buf.len().checked_add(packet.len()).ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, "UDP packet length overflow")
        })?;
        if encoded_len > PACKET_BUFFER_LENGTH {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "encoded UDP probe packet is too large",
            )
            .into());
        }
        transform_buf.clear();
        transform_buf.extend_from_slice(&header_buf);
        transform_buf.extend_from_slice(&packet);
        std::mem::swap(&mut packet, &mut transform_buf);
    }
    let start = Instant::now();
    upstream_write
        .trait_send(&packet)
        .await
        .map_err(io::Error::other)?;
    pkt_buf.resize(PACKET_BUFFER_LENGTH, 0);
    let n = upstream_read
        .trait_recv(pkt_buf)
        .await
        .map_err(io::Error::other)?;
    pkt_buf.truncate(n);
    let end = Instant::now();
    let mut packet = pkt_buf[..n].to_vec();
    for (index, node) in proxies.iter().enumerate() {
        trace!(?node.address, "Reading response");
        let validator = ValidatorRef::Time(&context.time_validator);
        let mut reader = io::Cursor::new(&packet);
        let resp: RouteResponse = read_header(&mut reader, *node.header_crypto.key(), &validator)?;
        if let Err(err) = resp.result {
            warn!(?err, %node.address, "Upstream responded with an error");
            return Err(TraceError::Response {
                err,
                addr: node.address.address.clone(),
            });
        }
        let consumed = usize::try_from(reader.position()).unwrap();
        let remaining = &packet[consumed..];
        if index + 1 < proxies.len()
            && let Some(payload_crypto) = &node.payload_crypto
        {
            transform_buf.resize(PACKET_BUFFER_LENGTH, 0);
            let decrypted = decrypt_packet_payload(remaining, &mut transform_buf, payload_crypto)?;
            packet = decrypted.to_vec();
        } else {
            packet = remaining.to_vec();
        }
    }
    // Explicit epilog: close the probe's write half now that its response
    // is in hand, so the echo flow on the proxy sees a clean EOF and
    // releases its session slot immediately instead of idling out and
    // tripping the flow's safety timeout.
    let outcome = upstream_write
        .trait_shutdown()
        .await
        .map_err(io::Error::other)?;
    counter!("udp.traces").increment(1);
    // The probe flow's termination, reported through the completion signal.
    // The epilog has already returned, so the flow's end is observed by
    // the epilog future, which keeps the read half and awaits the peer's
    // close. The future is returned unspawned so the caller owns it.
    let (end_tx, end_rx) = watch::channel(ProbeFlowEnd::Pending);
    let fut: Option<std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>>> = match outcome
    {
        ShutdownOutcome::Propagated => {
            // The epilog signalled EOF to the peer; the flow terminates
            // when the peer's response writer closes, observed as a
            // clean EOF on the probe's read half. The caller spawns
            // this future into its scoped JoinSet so the teardown is
            // owned and reaped; Eof and TimedOut are only reported
            // from here, after the epilog observes them.
            Some(Box::pin(async move {
                let end = match tokio::time::timeout(
                    FLOW_END_TIMEOUT,
                    await_probe_flow_end(&mut upstream_read),
                )
                .await
                {
                    Ok(true) => ProbeFlowEnd::Eof,
                    Ok(false) | Err(_) => ProbeFlowEnd::TimedOut,
                };
                let _ = end_tx.send_replace(end);
            }))
        }
        ShutdownOutcome::Unsupported => {
            // A raw-UDP hop cannot signal EOF: no EOF can arrive on the
            // probe's read half, and the flow is retained until its
            // safety timeout. Report the unsupported-shutdown end
            // immediately; there is no end to observe, so no epilog
            // future.
            let _ = end_tx.send_replace(ProbeFlowEnd::ShutdownUnsupported);
            None
        }
    };
    Ok((end.duration_since(start), ProbeEpilog { end: end_rx, fut }))
}
#[derive(Debug, Error)]
pub enum TraceError {
    #[error("IO error: {0}")]
    Io(#[from] io::Error),
    #[error("Failed to read response from upstream: {0}")]
    Header(#[from] CodecError),
    #[error("Upstream responded with an error: {err}, {addr}")]
    Response { err: RouteError, addr: InternetAddr },
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::net::UdpSocket;

    #[tokio::test]
    async fn a_response_longer_than_the_callers_buffer_is_capped_not_a_panic() {
        let peer = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let client = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        client.connect(peer.local_addr().unwrap()).await.unwrap();
        peer.connect(client.local_addr().unwrap()).await.unwrap();
        let connection = crate::proxy_runtime::connect::udp::UdpConnection::socket(client);
        let local_addr = connection.local_addr();
        let peer_addr = connection.peer_addr();
        let (upstream, _write) = connection.into_split();
        let mut read = UdpProxyClientReadHalf::new(
            upstream,
            local_addr,
            peer_addr,
            [].into(),
            Arc::new(RouteConfirmation::default()),
        );
        let mut buf = [0u8; 64];
        peer.send(&vec![0xab; buf.len() + 1]).await.unwrap();
        let n = read.recv(&mut buf).await.unwrap();
        assert_eq!(n, buf.len(), "a payload that does not fit must be capped");
        assert_eq!(buf, [0xab; 64]);
    }

    #[tokio::test]
    async fn a_valid_response_switches_later_requests_to_compact_form() {
        use crate::proxy_runtime::conn::udp::UDP_FLOW_ID_LEN;
        use crate::proxy_runtime::route_header::udp::{UdpRequestRoute, decode_request_route};

        let mut pkt = [0u8; 2048];

        let peer = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let client = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        client.connect(peer.local_addr().unwrap()).await.unwrap();
        peer.connect(client.local_addr().unwrap()).await.unwrap();
        let connection = crate::proxy_runtime::connect::udp::UdpConnection::socket(client);
        let local_addr = connection.local_addr();
        let peer_addr = connection.peer_addr();
        let (upstream_read, upstream_write) = connection.into_split();

        let crypto = tokio_chacha20::config::Config::new([7; tokio_chacha20::KEY_BYTES].into());
        let node = crate::route::HopConfig {
            address: RouteAddr::udp("127.0.0.1:9".parse::<SocketAddr>().unwrap().into()),
            header_crypto: crypto.clone(),
            payload_crypto: None,
        };
        let proxies: Arc<RouteChain> = Arc::new([node]);
        let layer_crypto = crypto.clone();
        let layer = UdpRequestLayer {
            header: RegeneratingHeader::new(
                Box::new(move || {
                    let mut buf = Vec::new();
                    write_header(
                        &mut buf,
                        &RouteRequest {
                            upstream: Some(RouteAddr::udp(
                                "127.0.0.1:9".parse::<SocketAddr>().unwrap().into(),
                            )),
                        },
                        *layer_crypto.key(),
                    )
                    .unwrap();
                    buf.into()
                }),
                VALIDATOR_UDP_HDR_TTL,
            ),
            payload_crypto: None,
        };
        let route_confirmation = Arc::new(RouteConfirmation::default());
        let flow_id = UdpFlowId::from_bytes([5; UDP_FLOW_ID_LEN]);
        let mut write = UdpProxyClientWriteHalf::new(
            upstream_write,
            peer_addr,
            vec![layer],
            flow_id,
            route_confirmation.clone(),
        );
        let mut read = UdpProxyClientReadHalf::new(
            upstream_read,
            local_addr,
            peer_addr,
            proxies,
            route_confirmation.clone(),
        );
        let decode = |pkt: &[u8]| -> (UdpRequestRoute, Vec<u8>) {
            let mut cursor = io::Cursor::new(pkt);
            let route = decode_request_route(&mut cursor, &crypto, &time_validator()).unwrap();
            let payload = pkt[cursor.position() as usize..].to_vec();
            (route, payload)
        };

        // (a) Before any response, requests are routed with the full header.
        write.send(b"first").await.unwrap();
        let n = peer.recv(&mut pkt).await.unwrap();
        let (route, payload) = decode(&pkt[..n]);
        match route {
            UdpRequestRoute::Routed { flow_id: id, .. } => assert_eq!(id, flow_id),
            other => panic!("expected a routed request, got {other:?}"),
        }
        assert_eq!(payload, b"first");

        // (b) A valid layered response confirms the route.
        let mut resp = Vec::new();
        write_header(&mut resp, &RouteResponse { result: Ok(()) }, *crypto.key()).unwrap();
        resp.extend_from_slice(b"pong");
        peer.send(&resp).await.unwrap();
        let mut out = [0u8; 64];
        let n = read.recv(&mut out).await.unwrap();
        assert_eq!(&out[..n], b"pong");

        // (c) Later requests switch to compact form on the same flow id.
        write.send(b"second").await.unwrap();
        let n = peer.recv(&mut pkt).await.unwrap();
        let (route, payload) = decode(&pkt[..n]);
        match route {
            UdpRequestRoute::Compact { flow_id: id } => assert_eq!(id, flow_id),
            other => panic!("expected a compact request, got {other:?}"),
        }
        assert_eq!(payload, b"second");

        // (d) Aging only the monotonic clock forces routed form again.
        {
            let mut guard = route_confirmation.last_response.lock().unwrap();
            let confirmed = guard.take().unwrap();
            *guard = Some(ConfirmationTime {
                monotonic: confirmed.monotonic - UDP_FLOW_TIMEOUT,
                wall: confirmed.wall,
            });
        }
        assert!(!route_confirmation.is_fresh());
        write.send(b"third").await.unwrap();
        let n = peer.recv(&mut pkt).await.unwrap();
        let (route, payload) = decode(&pkt[..n]);
        match route {
            UdpRequestRoute::Routed { flow_id: id, .. } => assert_eq!(id, flow_id),
            other => panic!("expected a routed request after monotonic aging, got {other:?}"),
        }
        assert_eq!(payload, b"third");

        // (e) Aging only the wall clock also forces routed form again.
        {
            let mut guard = route_confirmation.last_response.lock().unwrap();
            *guard = Some(ConfirmationTime {
                monotonic: Instant::now(),
                wall: SystemTime::now() - UDP_FLOW_TIMEOUT,
            });
        }
        assert!(!route_confirmation.is_fresh());
        write.send(b"fourth").await.unwrap();
        let n = peer.recv(&mut pkt).await.unwrap();
        let (route, payload) = decode(&pkt[..n]);
        match route {
            UdpRequestRoute::Routed { flow_id: id, .. } => assert_eq!(id, flow_id),
            other => panic!("expected a routed request after wall-clock aging, got {other:?}"),
        }
        assert_eq!(payload, b"fourth");
    }
}
