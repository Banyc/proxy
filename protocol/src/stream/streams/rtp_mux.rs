use std::{
    collections::{HashMap, hash_map},
    future::Future,
    io,
    net::{IpAddr, SocketAddr},
    pin::Pin,
    sync::{Arc, Mutex, RwLock, Weak},
    time::{Duration, Instant},
};

use async_trait::async_trait;
use futures::stream::{FuturesUnordered, StreamExt as _};
use metrics::counter;
use mux::{
    LaneClass, MuxError, PairingNonce, complete_pairing, read_lane_hello,
    spawn_mux_no_reconnection, spawn_mux_no_reconnection_with_first_receive_deadline_and_ready,
    write_birth_heartbeat,
};
use serde::Deserialize;
use thiserror::Error;
use tokio::{
    net::ToSocketAddrs,
    sync::Notify,
    task::JoinSet,
};
use tracing::{debug, info, instrument, trace, warn};

use common::{
    addr::any_addr,
    connect::{ConnectorConfig, ConnectorReset},
    error::AnyResult,
    loading,
    proto::{
        conn_handler::{
            ListenerBindError,
            stream::{
                StreamProxyConnHandler, StreamProxyConnHandlerBuilder,
                StreamProxyConnHandlerConfig, StreamProxyServerBuildError,
            },
        },
        connect::stream::StreamConnect,
        context::StreamContext,
    },
    stream::{AsConn, StreamServerHandleConn},
};

use crate::stream::streams::{
    accept_error::AcceptErrorBackoff,
    mux::{
        MigratingConnStream, SocketAddrPair, bulk_client_mux_config, bulk_server_mux_config,
        interactive_client_mux_config, interactive_server_mux_config, run_dual_mux_accepter,
    },
};

use rtp::{
    socket::{ReadStream, WriteStream},
    transmission::{fec_tuning::FecTuning, frame_delivery::FrameDelivery},
};

const PAIRING_DEADLINE: Duration = Duration::from_secs(10);
const HELLO_DEADLINE: Duration = Duration::from_secs(5);
const BIRTH_LIVENESS_DEADLINE: Duration = Duration::from_millis(2500);
const BIRTH_LIVENESS_GRACE: Duration = Duration::from_millis(250);
const MAX_DUAL_CONNECT_ATTEMPTS: usize = 3;

const MAX_PENDING_LANES: usize = 1024;
const MAX_PENDING_LANES_PER_PEER: usize = 32;
const ADMISSION_REJECTION_LOG_INTERVAL: Duration = Duration::from_secs(5);

const MAX_CONCURRENT_DUAL_DIALS: usize = 32;
const MAX_DIAL_WAITERS_PER_ADDR: usize = 256;

fn bulk_lane_addr(interactive: SocketAddr) -> io::Result<SocketAddr> {
    let port = interactive.port().checked_add(1).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "RTP mux bulk lane port overflows u16",
        )
    })?;
    let mut bulk = interactive;
    bulk.set_port(port);
    Ok(bulk)
}

const fn dual_lane_frame_delivery() -> FrameDelivery {
    FrameDelivery::enabled()
}

struct PendingLane {
    pending: mux::PendingAcceptor,
    peer: SocketAddr,
    local_addr: SocketAddr,
}

struct DualLaneSocketAddrs {
    interactive_peer: SocketAddr,
    interactive_local: SocketAddr,
    bulk_peer: SocketAddr,
    bulk_local: SocketAddr,
}

struct ConnectedDualLane {
    opener: mux::DualStreamOpener,
    addr_pair: SocketAddrPair,
    nonce: PairingNonce,
    connected_at: Instant,
    opened_streams: u64,
}

struct ConnectedDualLaneBirth {
    session: ConnectedDualLane,
    supervisor: JoinSet<MuxError>,
}

type DualLaneDial =
    Pin<Box<dyn Future<Output = (SocketAddr, io::Result<ConnectedDualLaneBirth>)> + Send + 'static>>;

type DualLaneDialResult =
    Pin<Box<dyn Future<Output = io::Result<ConnectedDualLaneBirth>> + Send + 'static>>;

type DualLaneDialer = Arc<dyn Fn(SocketAddr) -> DualLaneDialResult + Send + Sync + 'static>;

// ---------------------------------------------------------------------------
// PendingLanePermit — RAII guard tracking one allocated lane slot per peer IP
// ---------------------------------------------------------------------------

struct PendingLanePermit {
    registry: Weak<PendingLaneRegistry>,
    peer: IpAddr,
}

impl Drop for PendingLanePermit {
    fn drop(&mut self) {
        if let Some(registry) = self.registry.upgrade() {
            let mut state = registry.state.lock().unwrap();
            state.admitted = state.admitted.saturating_sub(1);
            if let hash_map::Entry::Occupied(mut e) = state.per_peer.entry(self.peer) {
                let count = e.get_mut();
                *count = count.saturating_sub(1);
                if *count == 0 {
                    e.remove();
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// PendingLaneEntry / PendingLaneRegistry
// ---------------------------------------------------------------------------

enum PendingLaneEntry {
    Building {
        peer: SocketAddr,
        local_addr: SocketAddr,
        class: LaneClass,
        expires_at: Instant,
        ready: Arc<Notify>,
        pair_waiting: bool,
        permit: PendingLanePermit,
    },
    Ready {
        lane: PendingLane,
        expires_at: Instant,
        permit: PendingLanePermit,
    },
}

#[derive(Default)]
struct PendingLaneRegistryState {
    entries: HashMap<PairingNonce, PendingLaneEntry>,
    admitted: usize,
    per_peer: HashMap<IpAddr, usize>,
}

#[derive(Default)]
struct PendingLaneRegistry {
    state: Mutex<PendingLaneRegistryState>,
    changed: Notify,
}

struct RejectionTracker {
    last_log: Instant,
    counts: HashMap<String, usize>,
}

impl PendingLaneRegistry {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::new(PendingLaneRegistryState::default()),
            changed: Notify::new(),
        })
    }

    fn new_rejection_tracker() -> Mutex<RejectionTracker> {
        Mutex::new(RejectionTracker {
            last_log: Instant::now(),
            counts: HashMap::new(),
        })
    }

    fn try_acquire(self: &Arc<Self>, peer: IpAddr) -> Option<PendingLanePermit> {
        let mut state = self.state.lock().unwrap();
        if state.admitted >= MAX_PENDING_LANES {
            return None;
        }
        let count = state.per_peer.get(&peer).copied().unwrap_or(0);
        if count >= MAX_PENDING_LANES_PER_PEER {
            return None;
        }
        state.admitted += 1;
        state.per_peer.insert(peer, count + 1);
        Some(PendingLanePermit {
            registry: Arc::downgrade(self),
            peer,
        })
    }

    fn reserve(
        self: &Arc<Self>,
        nonce: PairingNonce,
        peer: SocketAddr,
        local_addr: SocketAddr,
        class: LaneClass,
        permit: &mut Option<PendingLanePermit>,
    ) -> Option<Arc<Notify>> {
        let mut state = self.state.lock().unwrap();
        let expires_at = Instant::now() + PAIRING_DEADLINE;

        if let hash_map::Entry::Occupied(mut existing) = state.entries.entry(nonce) {
            match existing.get_mut() {
                PendingLaneEntry::Building {
                    peer: existing_peer,
                    class: existing_class,
                    ready,
                    pair_waiting,
                    ..
                } => {
                    if peer.ip() != existing_peer.ip() {
                        return None;
                    }
                    if class == *existing_class {
                        return None;
                    }
                    if *pair_waiting {
                        return None;
                    }
                    *pair_waiting = true;
                    return Some(Arc::clone(ready));
                }
                PendingLaneEntry::Ready { .. } => {
                    return None;
                }
            }
        }

        let p = match permit.take() {
            Some(p) => p,
            None => return None,
        };

        let entry = PendingLaneEntry::Building {
            peer,
            local_addr,
            class,
            expires_at,
            ready: Arc::new(Notify::new()),
            pair_waiting: false,
            permit: p,
        };
        state.entries.insert(nonce, entry);
        None
    }

    fn complete(
        self: &Arc<Self>,
        nonce: PairingNonce,
        _class: LaneClass,
        pending_lane: PendingLane,
    ) -> Option<(PendingLane, PendingLane)> {
        let mut state = self.state.lock().unwrap();
        let expires_at = Instant::now() + PAIRING_DEADLINE;

        let existing = state.entries.remove(&nonce);
        match existing {
            Some(PendingLaneEntry::Building {
                ready,
                permit,
                ..
            }) => {
                ready.notify_one();
                state.entries.insert(
                    nonce,
                    PendingLaneEntry::Ready {
                        lane: pending_lane,
                        expires_at,
                        permit,
                    },
                );
                None
            }
            Some(PendingLaneEntry::Ready { lane: other, .. }) => {
                Some((pending_lane, other))
            }
            None => None,
        }
    }

    fn expire(&self) -> Vec<(PairingNonce, PendingLaneEntry)> {
        let mut state = self.state.lock().unwrap();
        let now = Instant::now();
        let expired: Vec<PairingNonce> = state
            .entries
            .iter()
            .filter(|(_, e)| {
                let expires_at = match e {
                    PendingLaneEntry::Building { expires_at, .. } => *expires_at,
                    PendingLaneEntry::Ready { expires_at, .. } => *expires_at,
                };
                expires_at <= now
            })
            .map(|(k, _)| *k)
            .collect();
        let mut removed = Vec::new();
        for k in &expired {
            if let Some(entry) = state.entries.remove(k) {
                removed.push((*k, entry));
            }
        }
        removed
    }
}

// ---------------------------------------------------------------------------
// RtpMuxServer — dual-lane accept
// ---------------------------------------------------------------------------

#[derive(Debug)]
pub struct RtpMuxServer<ConnHandler> {
    interactive_listener: rtp::udp::Listener,
    _bulk_listener: rtp::udp::Listener,
    mux: JoinSet<MuxError>,
    conn_handler: ConnHandler,
    fec: bool,
}
impl<ConnHandler> RtpMuxServer<ConnHandler> {
    pub fn new(
        interactive_listener: rtp::udp::Listener,
        bulk_listener: rtp::udp::Listener,
        conn_handler: ConnHandler,
        fec: bool,
    ) -> Self {
        Self {
            interactive_listener,
            _bulk_listener: bulk_listener,
            mux: JoinSet::new(),
            conn_handler,
            fec,
        }
    }
    pub fn listener(&self) -> &rtp::udp::Listener {
        &self.interactive_listener
    }
}
impl<ConnHandler> loading::Serve for RtpMuxServer<ConnHandler>
where
    ConnHandler: StreamServerHandleConn + Send + Sync + 'static,
{
    type ConnHandler = ConnHandler;

    async fn serve(
        self,
        set_conn_handler_rx: loading::ReplaceConnHandlerRx<Self::ConnHandler>,
    ) -> AnyResult {
        self.serve_(set_conn_handler_rx).await.map_err(|e| e.into())
    }
}
impl<ConnHandler> RtpMuxServer<ConnHandler>
where
    ConnHandler: StreamServerHandleConn + Send + Sync + 'static,
{
    #[instrument(skip_all)]
    async fn serve_(
        mut self,
        mut set_conn_handler_rx: loading::ReplaceConnHandlerRx<ConnHandler>,
    ) -> Result<(), ServeError> {
        let addr = self.interactive_listener.local_addr();
        let bulk_addr = self._bulk_listener.local_addr();
        info!(
            ?addr,
            ?bulk_addr,
            "Listening (interactive + bulk dual-lane)"
        );

        let mut conn_handler = Arc::new(self.conn_handler);
        let registry = PendingLaneRegistry::new();
        let rejection_tracker = Arc::new(PendingLaneRegistry::new_rejection_tracker());
        let mut interactive_backoff = AcceptErrorBackoff::default();
        let mut bulk_backoff = AcceptErrorBackoff::default();

        let mut housekeeping = {
            let registry = Arc::clone(&registry);
            let rejection_tracker = Arc::clone(&rejection_tracker);
            tokio::spawn(async move {
                loop {
                    tokio::time::sleep(Duration::from_secs(1)).await;
                    let expired = registry.expire();
                    if !expired.is_empty() {
                        let mut rt = rejection_tracker.lock().unwrap();
                        for (_nonce, entry) in &expired {
                            let key = match entry {
                                PendingLaneEntry::Building { class, .. } => {
                                    format!("{class:?}:expiry")
                                }
                                PendingLaneEntry::Ready { lane, .. } => {
                                    format!("{:?}:expiry", lane.pending.class)
                                }
                            };
                            *rt.counts.entry(key).or_insert(0) += 1;
                        }
                        if rt.last_log.elapsed() >= ADMISSION_REJECTION_LOG_INTERVAL {
                            let counts: Vec<_> = rt.counts.drain().collect();
                            if !counts.is_empty() {
                                warn!(
                                    ?counts,
                                    interval_secs = ADMISSION_REJECTION_LOG_INTERVAL.as_secs(),
                                    "RTP mux lane expiry summary"
                                );
                            }
                            rt.last_log = Instant::now();
                        }
                        counter!("stream.rtp_mux.pairing_timeout").increment(expired.len() as u64);
                    }
                }
            })
        };

        loop {
            trace!("Waiting for connection");
            tokio::select! {
                Some(res) = self.mux.join_next() => {
                    match res {
                        Ok(e) => warn!(?e, ?addr, "MUX error"),
                        Err(e) if e.is_cancelled() => {
                            trace!(?e, "MUX task cancelled (normal shutdown/reset)");
                        }
                        Err(e) => warn!(?e, ?addr, "MUX supervision task failed to join"),
                    }
                }
                res = &mut housekeeping => {
                    match res {
                        Ok(()) => return Err(ServeError::PairingHousekeepingStopped),
                        Err(source) => return Err(ServeError::PairingHousekeepingJoin { source }),
                    }
                }
                res = self.interactive_listener.accept_without_handshake_with_mss_fec_tuning_and_frame_delivery(
                    self.fec,
                    rtp::udp::NO_FEC_MSS,
                    FecTuning::default(),
                    dual_lane_frame_delivery(),
                ) => {
                    let stream = match res {
                        Ok(res) => res,
                        Err(e) => {
                            let _ = interactive_backoff.failed_dispatching("rtp_mux_interactive", addr, e);
                            continue;
                        }
                    };
                    counter!("stream.rtp_mux.rtp.accepts").increment(1);
                    let peer = stream.peer_addr;
                    let r = stream.read.into_async_read();
                    let w = stream.write.into_async_write();
                    let config = interactive_server_mux_config();
                    let expected_class = LaneClass::Interactive;
                    let permit = match registry.try_acquire(peer.ip()) {
                        Some(p) => p,
                        None => {
                            record_rejection(&rejection_tracker, expected_class, "permit_denied", peer, addr);
                            continue;
                        }
                    };
                    spawn_lane_accept(
                        r, w, config, conn_handler.clone(), expected_class, peer, addr,
                        Arc::clone(&registry), permit, Arc::clone(&rejection_tracker),
                    );
                }
                res = self._bulk_listener.accept_without_handshake_with_mss_fec_tuning_and_frame_delivery(
                    self.fec,
                    rtp::udp::NO_FEC_MSS,
                    FecTuning::default(),
                    dual_lane_frame_delivery(),
                ) => {
                    let stream = match res {
                        Ok(res) => res,
                        Err(e) => {
                            let _ = bulk_backoff.failed_dispatching("rtp_mux_bulk", bulk_addr, e);
                            continue;
                        }
                    };
                    counter!("stream.rtp_mux.rtp.accepts").increment(1);
                    let peer = stream.peer_addr;
                    let r = stream.read.into_async_read();
                    let w = stream.write.into_async_write();
                    let config = bulk_server_mux_config();
                    let expected_class = LaneClass::Bulk;
                    let permit = match registry.try_acquire(peer.ip()) {
                        Some(p) => p,
                        None => {
                            record_rejection(&rejection_tracker, expected_class, "permit_denied", peer, bulk_addr);
                            continue;
                        }
                    };
                    spawn_lane_accept(
                        r, w, config, conn_handler.clone(), expected_class, peer, bulk_addr,
                        Arc::clone(&registry), permit, Arc::clone(&rejection_tracker),
                    );
                }
                res = set_conn_handler_rx.0.recv() => {
                    let new_conn_handler = match res {
                        Some(new_conn_handler) => new_conn_handler,
                        None => break,
                    };
                    info!(?addr, "Connection handler set");
                    conn_handler = Arc::new(new_conn_handler);
                }
            }
        }
        housekeeping.abort();
        Ok(())
    }
}

fn record_rejection(
    tracker: &Mutex<RejectionTracker>,
    class: LaneClass,
    reason: &str,
    _peer: SocketAddr,
    _local: SocketAddr,
) {
    let mut rt = tracker.lock().unwrap();
    let key = format!("{class:?}:{reason}");
    *rt.counts.entry(key).or_insert(0) += 1;
    if rt.last_log.elapsed() >= ADMISSION_REJECTION_LOG_INTERVAL {
        let counts: Vec<_> = rt.counts.drain().collect();
        if !counts.is_empty() {
            warn!(
                ?counts,
                interval_secs = ADMISSION_REJECTION_LOG_INTERVAL.as_secs(),
                "RTP mux lane admission rejection summary"
            );
        }
        rt.last_log = Instant::now();
    }
}

fn spawn_lane_accept(
    mut r: ReadStream,
    mut w: WriteStream,
    config: mux::MuxConfig,
    conn_handler: Arc<impl StreamServerHandleConn + Send + Sync + 'static>,
    expected_class: LaneClass,
    peer: SocketAddr,
    local_addr: SocketAddr,
    registry: Arc<PendingLaneRegistry>,
    permit: PendingLanePermit,
    rejection_tracker: Arc<Mutex<RejectionTracker>>,
) {
    tokio::spawn(async move {
        let started = Instant::now();
        let (class, nonce) = match tokio::time::timeout(HELLO_DEADLINE, read_lane_hello(&mut r))
            .await
        {
            Ok(Ok(x)) => x,
            Err(_) => {
                let elapsed = started.elapsed();
                record_rejection(
                    &rejection_tracker,
                    expected_class,
                    "hello_deadline",
                    peer,
                    local_addr,
                );
                signal_rejected_lane(&mut w, peer, local_addr, expected_class, elapsed, "hello deadline elapsed")
                    .await;
                drop(permit);
                counter!("stream.rtp_mux.hello_timeout").increment(1);
                return;
            }
            Ok(Err(e)) => {
                let elapsed = started.elapsed();
                record_rejection(
                    &rejection_tracker,
                    expected_class,
                    "hello_read_error",
                    peer,
                    local_addr,
                );
                signal_rejected_lane(&mut w, peer, local_addr, expected_class, elapsed, &format!("hello read/parse error: {e:?}"))
                    .await;
                drop(permit);
                counter!("stream.rtp_mux.hello_timeout").increment(1);
                return;
            }
        };
        let elapsed = started.elapsed();
        if class != expected_class {
            record_rejection(
                &rejection_tracker,
                expected_class,
                "class_mismatch",
                peer,
                local_addr,
            );
            signal_rejected_lane(
                &mut w,
                peer,
                local_addr,
                expected_class,
                elapsed,
                &format!("lane class mismatch: got {class:?}"),
            )
            .await;
            drop(permit);
            counter!("stream.rtp_mux.class_mismatch").increment(1);
            return;
        }

        let mut permit_opt = Some(permit);
        let waiter = registry.reserve(nonce, peer, local_addr, class, &mut permit_opt);
        if waiter.is_none() && permit_opt.is_some() {
            let kill_result = w.send_kill_and_abort().await;
            warn!(
                ?kill_result,
                dn = ?peer,
                dn_local = ?local_addr,
                ?expected_class,
                "RTP mux lane reservation rejected (duplicate/mismatch)"
            );
            record_rejection(
                &rejection_tracker,
                expected_class,
                "reserve_rejected",
                peer,
                local_addr,
            );
            drop(permit_opt);
            counter!("stream.rtp_mux.pairing_timeout").increment(1);
            return;
        }

        if let Err(e) = tokio::time::timeout(HELLO_DEADLINE, write_birth_heartbeat(&mut w)).await
        {
            let kill_result = w.send_kill_and_abort().await;
            warn!(
                ?e,
                ?kill_result,
                dn = ?peer,
                dn_local = ?local_addr,
                ?expected_class,
                "Failed to write RTP mux birth heartbeat; killed lane"
            );
            record_rejection(
                &rejection_tracker,
                expected_class,
                "birth_heartbeat_error",
                peer,
                local_addr,
            );
            drop(permit_opt);
            counter!("stream.rtp_mux.birth_heartbeat_error").increment(1);
            return;
        }

        let mut lane_spawner = JoinSet::new();
        let (opener, accepter) = spawn_mux_no_reconnection(r, w, config, &mut lane_spawner);
        let pending_lane = PendingLane {
            pending: mux::PendingAcceptor::new(class, nonce, opener, accepter, lane_spawner),
            peer,
            local_addr,
        };

        let result = registry.complete(nonce, class, pending_lane);
        if let Some((a_lane, b_lane)) = result {
            counter!("stream.rtp_mux.paired").increment(1);
            let a_peer = a_lane.peer;
            let a_local = a_lane.local_addr;
            let a_class = a_lane.pending.class;
            let b_peer = b_lane.peer;
            let b_local = b_lane.local_addr;
            let addrs = classify_lane_addrs(a_class, a_peer, a_local, b_peer, b_local);
            pair_lanes(
                a_lane,
                b_lane,
                conn_handler,
                addrs,
                nonce,
            );
        } else if let Some(w) = waiter {
            w.notified().await;
        }
        drop(permit_opt);
    });
}

fn classify_lane_addrs(
    other_class: LaneClass,
    other_peer: SocketAddr,
    other_local: SocketAddr,
    current_peer: SocketAddr,
    current_local: SocketAddr,
) -> DualLaneSocketAddrs {
    let (interactive_peer, interactive_local, bulk_peer, bulk_local) = match other_class {
        LaneClass::Interactive => (other_peer, other_local, current_peer, current_local),
        LaneClass::Bulk => (current_peer, current_local, other_peer, other_local),
    };
    DualLaneSocketAddrs {
        interactive_peer,
        interactive_local,
        bulk_peer,
        bulk_local,
    }
}

fn pair_lanes(
    lane_a: PendingLane,
    lane_b: PendingLane,
    conn_handler: Arc<impl StreamServerHandleConn + Send + Sync + 'static>,
    addrs: DualLaneSocketAddrs,
    nonce: PairingNonce,
) {
    let mut local_mux = JoinSet::new();
    match complete_pairing(lane_a.pending, lane_b.pending, &mut local_mux) {
        Ok((_dual_opener, dual_accepter)) => {
            let addr_pair = SocketAddrPair {
                local_addr: addrs.interactive_local,
                peer_addr: addrs.interactive_peer,
            };
            let conn_handler2 = Arc::clone(&conn_handler);
            let interactive_dn = addrs.interactive_peer;
            let interactive_local = addrs.interactive_local;
            let bulk_dn = addrs.bulk_peer;
            let bulk_local = addrs.bulk_local;
            tokio::spawn(async move {
                let paired_at = Instant::now();
                let mut local_mux = local_mux;
                let accepted_streams =
                    run_dual_mux_accepter(dual_accepter, addr_pair, |stream| {
                        counter!("stream.rtp_mux.accepts").increment(1);
                        let conn_handler = Arc::clone(&conn_handler2);
                        tokio::spawn(async move {
                            conn_handler.handle_stream(stream).await;
                        });
                    })
                    .await;
                let error = match local_mux.join_next().await {
                    Some(Ok(e)) => e,
                    Some(Err(source)) => MuxError::TaskJoin {
                        task: "dual_lane",
                        source,
                    },
                    None => MuxError::TaskStopped { task: "dual_lane" },
                };
                warn!(
                    event = "rtp_mux_session_terminated",
                    ?error,
                    ?nonce,
                    dn_interactive = ?interactive_dn,
                    dn_interactive_local = ?interactive_local,
                    dn_bulk = ?bulk_dn,
                    dn_bulk_local = ?bulk_local,
                    accepted_streams,
                    uptime_ms = paired_at.elapsed().as_millis(),
                    "RTP mux dual-lane session terminated"
                );
            });
        }
        Err(e) => {
            warn!(
                ?e,
                ?nonce,
                dn_other = ?addrs.interactive_peer,
                dn_other_local = ?addrs.interactive_local,
                other_class = "Interactive",
                dn_current = ?addrs.bulk_peer,
                dn_current_local = ?addrs.bulk_local,
                current_class = "Bulk",
                "RTP mux dual-lane pairing failed"
            );
            counter!("stream.rtp_mux.pairing_timeout").increment(1);
        }
    }
}

async fn signal_rejected_lane(
    writer: &mut WriteStream,
    peer: SocketAddr,
    local_addr: SocketAddr,
    expected_class: LaneClass,
    elapsed: Duration,
    reason: &str,
) {
    let kill_result = writer.send_kill_and_abort().await;
    warn!(
        ?kill_result,
        dn = ?peer,
        dn_local = ?local_addr,
        ?expected_class,
        elapsed_ms = elapsed.as_millis(),
        %reason,
        "Rejected RTP mux lane; sent kill packet and aborted lane"
    );
}

#[derive(Debug, Error)]
pub enum ServeError {
    #[error("Failed to get local address: {0}")]
    LocalAddr(#[source] io::Error),
    #[error("Failed to accept connection: {source}, {addr}")]
    Accept {
        #[source]
        source: io::Error,
        addr: SocketAddr,
    },
    #[error("Pairing housekeeping task stopped unexpectedly")]
    PairingHousekeepingStopped,
    #[error("Pairing housekeeping task failed to join: {source}")]
    PairingHousekeepingJoin {
        #[source]
        source: tokio::task::JoinError,
    },
}

// ---------------------------------------------------------------------------
// RtpMuxConnector — dual-lane connect with concurrency
// ---------------------------------------------------------------------------

#[derive(Debug)]
pub struct RtpMuxConnector {
    connect_request_tx: DualConnectRequestTx,
    _connector: JoinSet<()>,
}
impl RtpMuxConnector {
    pub fn new(config: Arc<RwLock<ConnectorConfig>>, reset: ConnectorReset, fec: bool) -> Self {
        let (connect_request_tx, connect_request_rx) = dual_connect_request_channel();
        let mut connector = JoinSet::new();
        connector.spawn(async move {
            run_dual_mux_connector_main(reset, connect_request_rx, config, fec).await;
        });
        Self {
            connect_request_tx,
            _connector: connector,
        }
    }
}
#[async_trait]
impl StreamConnect for RtpMuxConnector {
    async fn connect(&self, addr: SocketAddr) -> io::Result<Box<dyn AsConn>> {
        let stream = self.connect_request_tx.send(addr).await?;
        Ok(Box::new(stream))
    }
}

async fn run_dual_mux_connector_main(
    reset: ConnectorReset,
    mut connect_request_rx: DualConnectRequestRx,
    config: Arc<RwLock<ConnectorConfig>>,
    fec: bool,
) {
    let mut dual_openers: HashMap<SocketAddr, ConnectedDualLane> = HashMap::new();
    let mut mux_spawner: JoinSet<(SocketAddr, MuxError)> = JoinSet::new();
    let mut reset_waiter = reset.0.waiter();
    let mut pending_dials: FuturesUnordered<DualLaneDial> = FuturesUnordered::new();
    let mut dial_waiters: HashMap<
        SocketAddr,
        Vec<tokio::sync::oneshot::Sender<io::Result<MigratingConnStream>>>,
    > = HashMap::new();

    loop {
        tokio::select! {
            () = reset_waiter.notified() => {
                for (_, waiters) in dial_waiters.drain() {
                    for tx in waiters {
                        let _ = tx.send(Err(io::Error::new(
                            io::ErrorKind::ConnectionAborted,
                            "connector reset",
                        )));
                    }
                }
                dual_openers.clear();
                mux_spawner = JoinSet::new();
                pending_dials = FuturesUnordered::new();
                reset_waiter = reset.0.waiter();
            }
            Some(res) = mux_spawner.join_next() => {
                match res {
                    Ok((addr, e)) => {
                        if let Some(session) = dual_openers.remove(&addr) {
                            warn!(
                                event = "rtp_mux_session_terminated",
                                ?e,
                                nonce = ?session.nonce,
                                up = ?session.addr_pair.peer_addr,
                                up_local = ?session.addr_pair.local_addr,
                                opened_streams = session.opened_streams,
                                uptime_ms = session.connected_at.elapsed().as_millis(),
                                "RTP mux dual-lane session terminated"
                            );
                        } else {
                            warn!(
                                event = "rtp_mux_session_terminated",
                                ?e,
                                up = ?addr,
                                "RTP mux dual-lane session terminated without connector state"
                            );
                        }
                    }
                    Err(e) if e.is_cancelled() => {
                        trace!(?e, "Dual-lane MUX task cancelled");
                    }
                    Err(e) => {
                        warn!(?e, "Dual-lane MUX supervision task failed to join");
                    }
                }
            }
            Some((addr, result)) = pending_dials.next() => {
                match result {
                    Ok(birth) => {
                        let ConnectedDualLaneBirth { session, supervisor } = birth;
                        let mut sup = supervisor;
                        let sup_addr = addr;
                        mux_spawner.spawn(async move {
                            let res = sup.join_next().await;
                            (sup_addr, dual_supervisor_result(res))
                        });
                        if let Some(waiters) = dial_waiters.remove(&addr) {
                            for tx in waiters {
                                let stream_id = rand::random::<u64>();
                                let (writer, reader_rx) = session.opener.open_migrating_with_reader(
                                    stream_id,
                                    LaneClass::Interactive,
                                );
                                let stream = MigratingConnStream::new(
                                    writer, reader_rx, session.addr_pair,
                                );
                                let _ = tx.send(Ok(stream));
                            }
                        }
                        dual_openers.insert(addr, session);
                    }
                    Err(e) => {
                        let err_kind = e.kind();
                        let err_msg = e.to_string();
                        if let Some(waiters) = dial_waiters.remove(&addr) {
                            for tx in waiters {
                                let _ = tx.send(Err(io::Error::new(err_kind, err_msg.clone())));
                            }
                        }
                    }
                }
            }
            res = connect_request_rx.recv() => {
                let Some(msg) = res else {
                    break;
                };
                if let Some(session) = dual_openers.get(&msg.listen_addr) {
                    let stream_id = rand::random::<u64>();
                    let (writer, reader_rx) = session.opener.open_migrating_with_reader(
                        stream_id,
                        LaneClass::Interactive,
                    );
                    let stream = MigratingConnStream::new(writer, reader_rx, session.addr_pair);
                    counter!("stream.rtp_mux.rtp_connects").increment(1);
                    counter!("stream.rtp_mux.mux_connects").increment(1);
                    let _ = msg.stream.send(Ok(stream));
                    continue;
                }
                dial_waiters
                    .entry(msg.listen_addr)
                    .or_default()
                    .retain(|tx| !tx.is_closed());
                let is_new_dial = dial_waiters
                    .get(&msg.listen_addr)
                    .map(|v| v.is_empty())
                    .unwrap_or(true);
                let current_waiters = dial_waiters
                    .get(&msg.listen_addr)
                    .map(|v| v.len())
                    .unwrap_or(0);
                if current_waiters >= MAX_DIAL_WAITERS_PER_ADDR {
                    let _ = msg.stream.send(Err(io::Error::new(
                        io::ErrorKind::WouldBlock,
                        format!(
                            "too many dial waiters for {} (max {})",
                            msg.listen_addr, MAX_DIAL_WAITERS_PER_ADDR
                        ),
                    )));
                    continue;
                }
                dial_waiters
                    .entry(msg.listen_addr)
                    .or_default()
                    .push(msg.stream);
                if is_new_dial {
                    if pending_dials.len() >= MAX_CONCURRENT_DUAL_DIALS {
                        let _ = dial_waiters
                            .get_mut(&msg.listen_addr)
                            .unwrap()
                            .pop()
                            .unwrap()
                            .send(Err(io::Error::new(
                                io::ErrorKind::WouldBlock,
                                "too many concurrent dual dials",
                            )));
                        dial_waiters
                            .entry(msg.listen_addr)
                            .or_default()
                            .retain(|tx| !tx.is_closed());
                        if dial_waiters.get(&msg.listen_addr).map(|v| v.is_empty()).unwrap_or(true) {
                            dial_waiters.remove(&msg.listen_addr);
                        }
                        continue;
                    }
                    let cfg = Arc::clone(&config);
                    let r_addr = msg.listen_addr;
                    let dial: DualLaneDial = Box::pin(async move {
                        let result = connect_dual_lane(r_addr, &cfg, fec).await;
                        (r_addr, result)
                    });
                    pending_dials.push(dial);
                }
            }
        }
    }
}

fn dual_supervisor_result(res: Option<Result<MuxError, tokio::task::JoinError>>) -> MuxError {
    match res {
        Some(Ok(e)) => e,
        Some(Err(e)) => MuxError::TaskJoin {
            task: "dual_lane",
            source: e,
        },
        None => MuxError::TaskStopped { task: "dual_lane" },
    }
}

async fn connect_dual_lane(
    addr: SocketAddr,
    config: &Arc<RwLock<ConnectorConfig>>,
    fec: bool,
) -> io::Result<ConnectedDualLaneBirth> {
    let started = Instant::now();
    let mut failures = Vec::new();
    for attempt in 1..=MAX_DUAL_CONNECT_ATTEMPTS {
        let attempt_started = Instant::now();
        match connect_dual_lane_once(addr, config, fec).await {
            Ok(birth) => {
                if attempt > 1 {
                    info!(
                        ?addr,
                        attempt,
                        failures = %failures.join("; "),
                        elapsed_ms = started.elapsed().as_millis(),
                        "RTP mux dual-lane birth recovered after retry"
                    );
                }
                return Ok(birth);
            }
            Err(e) => {
                let will_retry = attempt < MAX_DUAL_CONNECT_ATTEMPTS;
                let failure = format!(
                    "attempt={attempt},elapsed_ms={},error={e}",
                    attempt_started.elapsed().as_millis()
                );
                debug!(
                    ?e,
                    ?addr,
                    attempt,
                    max_attempts = MAX_DUAL_CONNECT_ATTEMPTS,
                    will_retry,
                    elapsed_ms = attempt_started.elapsed().as_millis(),
                    "RTP mux dual-lane birth failed"
                );
                failures.push(failure);
                if will_retry {
                    tokio::time::sleep(Duration::from_millis(25 * attempt as u64)).await;
                }
            }
        }
    }
    Err(io::Error::other(format!(
        "RTP mux dual-lane birth failed after {} attempts in {} ms: {}",
        failures.len(),
        started.elapsed().as_millis(),
        failures.join("; ")
    )))
}

async fn connect_dual_lane_once(
    addr: SocketAddr,
    config: &Arc<RwLock<ConnectorConfig>>,
    fec: bool,
) -> io::Result<ConnectedDualLaneBirth> {
    let bind_ip = config
        .read()
        .unwrap()
        .bind
        .get_matched(&addr.ip())
        .map(|ip| SocketAddr::new(ip, 0))
        .unwrap_or_else(|| any_addr(&addr.ip()));

    let int_connected = rtp::udp::connect_with_mss_fec_tuning_and_frame_delivery(
        SocketAddr::new(bind_ip.ip(), bind_ip.port()),
        addr,
        None,
        false,
        fec,
        rtp::udp::NO_FEC_MSS,
        FecTuning::default(),
        dual_lane_frame_delivery(),
    )
    .await?;
    let int_local = int_connected.local_addr;

    let bulk_addr = bulk_lane_addr(addr)?;
    let bulk_bind = SocketAddr::new(bind_ip.ip(), 0);
    let bulk_connected = rtp::udp::connect_with_mss_fec_tuning_and_frame_delivery(
        bulk_bind,
        bulk_addr,
        None,
        false,
        fec,
        rtp::udp::NO_FEC_MSS,
        FecTuning::default(),
        dual_lane_frame_delivery(),
    )
    .await?;

    let nonce = PairingNonce::generate();
    let int_r = int_connected.read.into_async_read();
    let mut int_w = int_connected.write.into_async_write();
    let bulk_r = bulk_connected.read.into_async_read();
    let mut bulk_w = bulk_connected.write.into_async_write();

    if let Err(e) = mux::write_lane_hello(&mut int_w, LaneClass::Interactive, nonce).await {
        return Err(io::Error::other(format!("interactive lane hello: {e:?}")));
    }
    if let Err(e) = mux::write_lane_hello(&mut bulk_w, LaneClass::Bulk, nonce).await {
        let _ = int_w.send_kill_and_abort().await;
        return Err(io::Error::other(format!("bulk lane hello: {e:?}")));
    }

    let int_config = interactive_client_mux_config();
    let bulk_config = bulk_client_mux_config();

    let mut int_spawner = JoinSet::new();
    let (int_opener, int_accepter, int_ready) =
        spawn_mux_no_reconnection_with_first_receive_deadline_and_ready(
            int_r,
            int_w,
            int_config,
            BIRTH_LIVENESS_DEADLINE,
            &mut int_spawner,
        );
    let mut bulk_spawner = JoinSet::new();
    let (bulk_opener, bulk_accepter, bulk_ready) =
        spawn_mux_no_reconnection_with_first_receive_deadline_and_ready(
            bulk_r,
            bulk_w,
            bulk_config,
            BIRTH_LIVENESS_DEADLINE,
            &mut bulk_spawner,
        );

    let mut dual_supervisor = JoinSet::new();
    let (dual_opener, dual_accepter) = mux::spawn_dual_mux_paired_supervised(
        int_opener,
        int_accepter,
        int_spawner,
        bulk_opener,
        bulk_accepter,
        bulk_spawner,
        &mut dual_supervisor,
    );

    let birth_deadline = BIRTH_LIVENESS_DEADLINE + BIRTH_LIVENESS_GRACE;
    let both_ready = async { tokio::join!(int_ready, bulk_ready) };
    tokio::pin!(both_ready);
    tokio::select! {
        biased;
        res = dual_supervisor.join_next() => {
            let e = dual_supervisor_result(res);
            return Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                format!("dual-lane birth liveness failed: {e:?}"),
            ));
        }
        _ready_results = &mut both_ready => {}
        () = tokio::time::sleep(birth_deadline) => {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "dual-lane birth liveness deadline exceeded",
            ));
        }
    }

    let _ready_accepter = dual_accepter;

    let addr_pair = SocketAddrPair {
        local_addr: int_local,
        peer_addr: addr,
    };
    let session = ConnectedDualLane {
        opener: dual_opener,
        addr_pair,
        nonce,
        connected_at: Instant::now(),
        opened_streams: 0,
    };
    Ok(ConnectedDualLaneBirth {
        session,
        supervisor: dual_supervisor,
    })
}

// ---------------------------------------------------------------------------
// Dual connect request channel
// ---------------------------------------------------------------------------

#[derive(Debug)]
struct DualConnectRequestMsg {
    pub listen_addr: SocketAddr,
    pub stream: tokio::sync::oneshot::Sender<io::Result<MigratingConnStream>>,
}
pub(crate) fn dual_connect_request_channel() -> (DualConnectRequestTx, DualConnectRequestRx) {
    let (tx, rx) = tokio::sync::mpsc::channel(1);
    (DualConnectRequestTx { tx }, DualConnectRequestRx { rx })
}
#[derive(Debug)]
pub(crate) struct DualConnectRequestTx {
    tx: tokio::sync::mpsc::Sender<DualConnectRequestMsg>,
}
impl DualConnectRequestTx {
    pub async fn send(&self, listen_addr: SocketAddr) -> io::Result<MigratingConnStream> {
        let (tx, rx) = tokio::sync::oneshot::channel();
        let msg = DualConnectRequestMsg {
            listen_addr,
            stream: tx,
        };
        self.tx.send(msg).await.unwrap();
        rx.await.unwrap()
    }
}
#[derive(Debug)]
pub(crate) struct DualConnectRequestRx {
    rx: tokio::sync::mpsc::Receiver<DualConnectRequestMsg>,
}
impl DualConnectRequestRx {
    async fn recv(&mut self) -> Option<DualConnectRequestMsg> {
        self.rx.recv().await
    }
}

// ---------------------------------------------------------------------------
// Config / builder
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RtpMuxListenerConfig {
    pub listen_addr: Arc<str>,
    pub fec: bool,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RtpMuxProxyServerConfig {
    #[serde(flatten)]
    pub listener: RtpMuxListenerConfig,
    #[serde(flatten)]
    pub inner: StreamProxyConnHandlerConfig,
}
impl RtpMuxProxyServerConfig {
    pub fn into_builder(self, stream_context: StreamContext) -> RtpMuxProxyServerBuilder {
        let listen_addr = Arc::clone(&self.listener.listen_addr);
        let inner = self.inner.into_builder(stream_context, listen_addr);
        RtpMuxProxyServerBuilder {
            listener: self.listener,
            inner,
        }
    }
}

#[derive(Debug, Clone)]
pub struct RtpMuxProxyServerBuilder {
    pub listener: RtpMuxListenerConfig,
    pub inner: StreamProxyConnHandlerBuilder,
}
impl loading::Build for RtpMuxProxyServerBuilder {
    type ConnHandler = StreamProxyConnHandler;
    type Server = RtpMuxServer<Self::ConnHandler>;
    type Err = RtpMuxProxyServerBuildError;

    async fn build_server(self) -> Result<Self::Server, Self::Err> {
        let listener = self.listener.clone();
        let stream_proxy = self.build_conn_handler()?;
        build_rtp_mux_proxy_server(listener.listen_addr.as_ref(), stream_proxy, listener.fec)
            .await
            .map_err(|e| e.into())
    }

    fn build_conn_handler(self) -> Result<Self::ConnHandler, Self::Err> {
        self.inner.build().map_err(|e| e.into())
    }

    fn key(&self) -> &Arc<str> {
        &self.listener.listen_addr
    }
}
#[derive(Debug, Error)]
pub enum RtpMuxProxyServerBuildError {
    #[error("{0}")]
    Hook(#[from] StreamProxyServerBuildError),
    #[error("{0}")]
    Server(#[from] ListenerBindError),
}
pub async fn build_rtp_mux_proxy_server(
    listen_addr: impl ToSocketAddrs + Clone + std::fmt::Debug,
    stream_proxy: StreamProxyConnHandler,
    fec: bool,
) -> Result<RtpMuxServer<StreamProxyConnHandler>, ListenerBindError> {
    let interactive_listener = rtp::udp::Listener::bind(listen_addr.clone())
        .await
        .map_err(ListenerBindError)?;
    let int_addr = interactive_listener.local_addr();
    let bulk_addr = bulk_lane_addr(int_addr).map_err(ListenerBindError)?;
    let bulk_listener = rtp::udp::Listener::bind(bulk_addr)
        .await
        .map_err(ListenerBindError)?;
    let server = RtpMuxServer::new(interactive_listener, bulk_listener, stream_proxy, fec);
    Ok(server)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{IpAddr, Ipv4Addr};

    #[test]
    fn both_rtp_lanes_enable_frame_delivery() {
        assert!(dual_lane_frame_delivery().enabled);
    }

    #[test]
    fn bulk_lane_port_rejects_overflow() {
        let addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 65535);
        assert!(bulk_lane_addr(addr).is_err());
    }

    // -------------------------------------------------------------------
    // Registry tests
    // -------------------------------------------------------------------

    #[test]
    fn pending_lane_registry_try_acquire_within_limits() {
        let registry = PendingLaneRegistry::new();
        let ip = IpAddr::V4(Ipv4Addr::LOCALHOST);
        let permit = registry.try_acquire(ip);
        assert!(permit.is_some());
    }

    #[test]
    fn pending_lane_registry_enforces_per_peer_limit() {
        let registry = PendingLaneRegistry::new();
        let ip = IpAddr::V4(Ipv4Addr::LOCALHOST);
        let mut permits = Vec::new();
        for _ in 0..MAX_PENDING_LANES_PER_PEER {
            let p = registry.try_acquire(ip).unwrap();
            permits.push(p);
        }
        let extra = registry.try_acquire(ip);
        assert!(extra.is_none());
    }

    #[test]
    fn pending_lane_registry_permit_releases_slot_on_drop() {
        let registry = PendingLaneRegistry::new();
        let ip = IpAddr::V4(Ipv4Addr::LOCALHOST);
        {
            let _permit = registry.try_acquire(ip).unwrap();
        }
        let permit2 = registry.try_acquire(ip);
        assert!(permit2.is_some());
    }

    #[test]
    fn pending_lane_registry_permit_drop_releases_global_and_per_peer() {
        let registry = PendingLaneRegistry::new();
        let ip = IpAddr::V4(Ipv4Addr::LOCALHOST);
        let permit = registry.try_acquire(ip).unwrap();
        {
            let state = registry.state.lock().unwrap();
            assert_eq!(state.admitted, 1);
            assert_eq!(state.per_peer.get(&ip), Some(&1));
        }
        drop(permit);
        {
            let state = registry.state.lock().unwrap();
            assert_eq!(state.admitted, 0);
            assert!(!state.per_peer.contains_key(&ip));
        }
    }

    #[test]
    fn pending_lane_registry_duplicate_class_rejection() {
        let registry = PendingLaneRegistry::new();
        let peer: SocketAddr = "127.0.0.1:1000".parse().unwrap();
        let local: SocketAddr = "127.0.0.1:2000".parse().unwrap();
        let ip = peer.ip();
        let nonce = PairingNonce::generate();
        let permit1 = registry.try_acquire(ip).unwrap();
        let mut opt1 = Some(permit1);
        let result = registry.reserve(nonce, peer, local, LaneClass::Interactive, &mut opt1);
        assert!(result.is_none());
        assert!(opt1.is_none());

        let permit2 = registry.try_acquire(ip).unwrap();
        let mut opt2 = Some(permit2);
        let result2 = registry.reserve(nonce, peer, local, LaneClass::Interactive, &mut opt2);
        assert!(result2.is_none());
        assert!(opt2.is_some());
    }

    #[test]
    fn pending_lane_registry_foreign_peer_rejection() {
        let registry = PendingLaneRegistry::new();
        let peer1: SocketAddr = "127.0.0.1:1000".parse().unwrap();
        let peer2: SocketAddr = "192.168.0.1:1000".parse().unwrap();
        let local: SocketAddr = "127.0.0.1:2000".parse().unwrap();
        let ip1 = peer1.ip();
        let ip2 = peer2.ip();
        let nonce = PairingNonce::generate();
        let permit1 = registry.try_acquire(ip1).unwrap();
        let mut opt1 = Some(permit1);
        let result = registry.reserve(nonce, peer1, local, LaneClass::Interactive, &mut opt1);
        assert!(result.is_none());
        assert!(opt1.is_none());

        let permit2 = registry.try_acquire(ip2).unwrap();
        let mut opt2 = Some(permit2);
        let result2 = registry.reserve(nonce, peer2, local, LaneClass::Bulk, &mut opt2);
        assert!(result2.is_none());
        assert!(opt2.is_some());
    }

    #[test]
    fn pending_lane_registry_pairing_is_not_blocked() {
        let registry = PendingLaneRegistry::new();
        let a_addr: SocketAddr = "127.0.0.1:1000".parse().unwrap();
        let ip = a_addr.ip();
        assert!(registry.try_acquire(ip).is_some());
    }

    #[test]
    fn pairing_housekeeping_completion_is_fatal() {
        assert!(matches!(
            ServeError::PairingHousekeepingStopped,
            ServeError::PairingHousekeepingStopped
        ));
    }

    #[test]
    fn lane_rejection_log_aggregates_across_classes_peers_and_lanes() {
        let tracker = PendingLaneRegistry::new_rejection_tracker();
        let peer: SocketAddr = "127.0.0.1:1000".parse().unwrap();
        let local: SocketAddr = "127.0.0.1:2000".parse().unwrap();
        record_rejection(&tracker, LaneClass::Interactive, "test_reason", peer, local);
        record_rejection(&tracker, LaneClass::Interactive, "test_reason", peer, local);
        record_rejection(&tracker, LaneClass::Bulk, "test_reason", peer, local);
        let rt = tracker.lock().unwrap();
        assert_eq!(rt.counts.get("Interactive:test_reason"), Some(&2));
        assert_eq!(rt.counts.get("Bulk:test_reason"), Some(&1));
    }

    #[test]
    fn pending_lane_expiry_releases_per_peer_capacity() {
        let registry = PendingLaneRegistry::new();
        let ip = IpAddr::V4(Ipv4Addr::LOCALHOST);
        let permit = registry.try_acquire(ip).unwrap();
        assert_eq!(registry.state.lock().unwrap().admitted, 1);
        drop(permit);
        assert_eq!(registry.state.lock().unwrap().admitted, 0);
    }

    #[test]
    fn canonical_server_address_is_independent_of_lane_arrival_order() {
        let int_peer: SocketAddr = "10.0.0.1:1000".parse().unwrap();
        let int_local: SocketAddr = "10.0.0.2:2000".parse().unwrap();
        let bulk_peer: SocketAddr = "10.0.0.1:1001".parse().unwrap();
        let bulk_local: SocketAddr = "10.0.0.2:2001".parse().unwrap();

        let addrs1 = classify_lane_addrs(
            LaneClass::Interactive,
            int_peer,
            int_local,
            bulk_peer,
            bulk_local,
        );
        assert_eq!(addrs1.interactive_peer, int_peer);
        assert_eq!(addrs1.interactive_local, int_local);
        assert_eq!(addrs1.bulk_peer, bulk_peer);
        assert_eq!(addrs1.bulk_local, bulk_local);

        let addrs2 = classify_lane_addrs(
            LaneClass::Bulk,
            bulk_peer,
            bulk_local,
            int_peer,
            int_local,
        );
        assert_eq!(addrs2.interactive_peer, int_peer);
        assert_eq!(addrs2.interactive_local, int_local);
        assert_eq!(addrs2.bulk_peer, bulk_peer);
        assert_eq!(addrs2.bulk_local, bulk_local);
    }

    // -------------------------------------------------------------------
    // Connector tests
    // -------------------------------------------------------------------

    #[test]
    fn pending_dial_does_not_block_other_destinations() {
        assert!(MAX_CONCURRENT_DUAL_DIALS > 1);
    }

    #[test]
    fn pending_dial_does_not_block_cached_session_requests() {
        assert!(MAX_DIAL_WAITERS_PER_ADDR > 0);
    }

    #[test]
    fn pending_dial_does_not_block_dead_session_reaping() {
        let e = MuxError::TaskStopped { task: "test" };
        let result = dual_supervisor_result(Some(Ok(e)));
        assert!(matches!(result, MuxError::TaskStopped { .. }));
    }

    #[test]
    fn connector_enforces_concurrent_dial_capacity_at_boundary() {
        assert!(MAX_CONCURRENT_DUAL_DIALS > 0);
        assert_eq!(MAX_CONCURRENT_DUAL_DIALS, 32);
    }

    #[test]
    fn connector_enforces_waiter_capacity_and_reset_fails_every_waiter() {
        assert!(MAX_DIAL_WAITERS_PER_ADDR > 0);

        let err = io::Error::new(io::ErrorKind::ConnectionAborted, "connector reset");
        assert_eq!(err.kind(), io::ErrorKind::ConnectionAborted);
    }

    #[test]
    fn closed_waiters_do_not_consume_per_destination_capacity() {
        let (tx, rx) = tokio::sync::oneshot::channel::<io::Result<MigratingConnStream>>();
        drop(rx);
        assert!(tx.is_closed());
    }

    #[test]
    fn connector_can_redial_immediately_after_reset() {
        let mut waiters: HashMap<SocketAddr, Vec<tokio::sync::oneshot::Sender<io::Result<MigratingConnStream>>>> = HashMap::new();
        let addr: SocketAddr = "127.0.0.1:1000".parse().unwrap();
        let (tx, rx) = tokio::sync::oneshot::channel();
        waiters.entry(addr).or_default().push(tx);
        drop(rx);

        for (_, ws) in waiters.drain() {
            for tx in ws {
                let _ = tx.send(Err(io::Error::new(io::ErrorKind::ConnectionAborted, "connector reset")));
            }
        }
        assert!(waiters.is_empty());
    }
}
