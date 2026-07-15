use std::{
    collections::{HashMap, HashSet, hash_map},
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
use tokio::{net::ToSocketAddrs, sync::Notify, task::JoinSet};
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

struct AdmittedLane {
    read: ReadStream,
    write: WriteStream,
    config: mux::MuxConfig,
    expected_class: LaneClass,
    peer: SocketAddr,
    local_addr: SocketAddr,
    permit: PendingLanePermit,
}

struct PendingLane {
    pending: mux::PendingAcceptor,
    peer: SocketAddr,
    local_addr: SocketAddr,
    _permit: PendingLanePermit,
}

struct PreparedLane {
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

type DualLaneDial = Pin<
    Box<dyn Future<Output = (SocketAddr, io::Result<ConnectedDualLaneBirth>)> + Send + 'static>,
>;

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
// PendingLaneAdmission – state machine for lane-pairing admission
// ---------------------------------------------------------------------------

enum PendingLaneAdmission {
    Reserved,
    Wait {
        ready: Arc<Notify>,
        expires_at: Instant,
    },
    Pair {
        lane: PendingLane,
        expires_at: Instant,
    },
    Reject(&'static str),
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
    },
}

enum ExpiredPendingLane {
    Building {
        nonce: PairingNonce,
        peer: SocketAddr,
        local_addr: SocketAddr,
        class: LaneClass,
        ready: Arc<Notify>,
        _permit: PendingLanePermit,
    },
    Ready {
        nonce: PairingNonce,
        lane: PendingLane,
    },
}

impl PendingLaneEntry {
    fn peer(&self) -> SocketAddr {
        match self {
            Self::Building { peer, .. } => *peer,
            Self::Ready { lane, .. } => lane.peer,
        }
    }
    fn class(&self) -> LaneClass {
        match self {
            Self::Building { class, .. } => *class,
            Self::Ready { lane, .. } => lane.pending.class,
        }
    }
    fn expires_at(&self) -> Instant {
        match self {
            Self::Building { expires_at, .. } | Self::Ready { expires_at, .. } => *expires_at,
        }
    }
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum LaneRejectionClass {
    Capacity,
    HelloTimeout,
    HelloParse,
    ClassMismatch,
    Admission,
    PairingTimeout,
    BirthHeartbeat,
    ReservationLost,
}

#[derive(Debug, Clone)]
struct RejectedLaneContext {
    class: LaneRejectionClass,
    peer: SocketAddr,
    local_addr: SocketAddr,
    expected_class: Option<LaneClass>,
    reason: String,
}

#[derive(Debug, Default)]
struct LaneRejectionSummary {
    total: u64,
    by_class: HashMap<LaneRejectionClass, u64>,
    first: Option<RejectedLaneContext>,
    last: Option<RejectedLaneContext>,
}

#[derive(Debug, Default)]
struct LaneRejectionLogInner {
    summary: Mutex<LaneRejectionSummary>,
}

#[derive(Debug, Clone, Default)]
struct LaneRejectionLog {
    inner: Arc<LaneRejectionLogInner>,
}

impl LaneRejectionLog {
    fn record(&self, context: RejectedLaneContext) {
        let mut summary = self.inner.summary.lock().unwrap();
        summary.total = summary.total.saturating_add(1);
        *summary.by_class.entry(context.class).or_default() = summary
            .by_class
            .get(&context.class)
            .copied()
            .unwrap_or_default()
            .saturating_add(1);
        summary.first.get_or_insert_with(|| context.clone());
        summary.last = Some(context);
    }
    fn flush(&self) {
        let summary = {
            let mut summary = self.inner.summary.lock().unwrap();
            if summary.total == 0 {
                return;
            }
            std::mem::take(&mut *summary)
        };
        let first = summary.first.unwrap();
        let last = summary.last.unwrap();
        warn!(
            event = "rtp_mux_lane_rejected",
            rejected = summary.total,
            rejection_classes = ?summary.by_class,
            first_class = ?first.class,
            first_dn = ?first.peer,
            first_dn_local = ?first.local_addr,
            first_expected_class = ?first.expected_class,
            first_reason = %first.reason,
            last_class = ?last.class,
            last_dn = ?last.peer,
            last_dn_local = ?last.local_addr,
            last_expected_class = ?last.expected_class,
            last_reason = %last.reason,
            "Rejected RTP mux lanes"
        );
    }
}

impl PendingLaneRegistry {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::new(PendingLaneRegistryState::default()),
            changed: Notify::new(),
        })
    }

    fn try_acquire(self: &Arc<Self>, peer: IpAddr) -> Result<PendingLanePermit, &'static str> {
        let mut state = self.state.lock().unwrap();
        if state.admitted >= MAX_PENDING_LANES {
            return Err("total pending lane capacity exhausted");
        }
        let count = state.per_peer.get(&peer).copied().unwrap_or(0);
        if count >= MAX_PENDING_LANES_PER_PEER {
            return Err("per-peer pending lane capacity exhausted");
        }
        state.admitted += 1;
        state.per_peer.insert(peer, count + 1);
        Ok(PendingLanePermit {
            registry: Arc::downgrade(self),
            peer,
        })
    }

    fn admit(
        &self,
        nonce: PairingNonce,
        class: LaneClass,
        peer: SocketAddr,
        local_addr: SocketAddr,
        permit: &mut Option<PendingLanePermit>,
    ) -> PendingLaneAdmission {
        let mut state = self.state.lock().unwrap();
        if let Some(entry) = state.entries.get_mut(&nonce) {
            if entry.peer().ip() != peer.ip() {
                return PendingLaneAdmission::Reject("pairing nonce peer mismatch");
            }
            if entry.class() == class {
                return PendingLaneAdmission::Reject("duplicate lane class for pairing nonce");
            }
            if let PendingLaneEntry::Building {
                ready,
                expires_at,
                pair_waiting,
                ..
            } = entry
            {
                if *pair_waiting {
                    return PendingLaneAdmission::Reject(
                        "opposite lane is already waiting for pairing",
                    );
                }
                *pair_waiting = true;
                return PendingLaneAdmission::Wait {
                    ready: Arc::clone(ready),
                    expires_at: *expires_at,
                };
            }
            if matches!(
                state.entries.get(&nonce),
                Some(PendingLaneEntry::Ready { .. })
            ) {
                let PendingLaneEntry::Ready { lane, expires_at } =
                    state.entries.remove(&nonce).unwrap()
                else {
                    unreachable!()
                };
                drop(state);
                self.changed.notify_one();
                return PendingLaneAdmission::Pair { lane, expires_at };
            }
            unreachable!()
        }
        let ready = Arc::new(Notify::new());
        state.entries.insert(
            nonce,
            PendingLaneEntry::Building {
                peer,
                local_addr,
                class,
                expires_at: Instant::now() + PAIRING_DEADLINE,
                ready,
                pair_waiting: false,
                permit: permit
                    .take()
                    .expect("accepted RTP mux lane must retain its admission permit"),
            },
        );
        drop(state);
        self.changed.notify_one();
        PendingLaneAdmission::Reserved
    }

    fn finish_reservation(
        &self,
        nonce: PairingNonce,
        lane: PreparedLane,
    ) -> Result<(), PreparedLane> {
        let mut state = self.state.lock().unwrap();
        let Some(entry) = state.entries.remove(&nonce) else {
            return Err(lane);
        };
        let PendingLaneEntry::Building {
            peer,
            local_addr,
            class,
            expires_at,
            ready,
            pair_waiting,
            permit,
        } = entry
        else {
            state.entries.insert(nonce, entry);
            return Err(lane);
        };
        if peer != lane.peer || class != lane.pending.class {
            state.entries.insert(
                nonce,
                PendingLaneEntry::Building {
                    peer,
                    local_addr,
                    class,
                    expires_at,
                    ready,
                    pair_waiting,
                    permit,
                },
            );
            return Err(lane);
        }
        let lane = PendingLane {
            pending: lane.pending,
            peer: lane.peer,
            local_addr: lane.local_addr,
            _permit: permit,
        };
        state
            .entries
            .insert(nonce, PendingLaneEntry::Ready { lane, expires_at });
        drop(state);
        ready.notify_one();
        self.changed.notify_one();
        Ok(())
    }

    fn restore_ready(&self, nonce: PairingNonce, lane: PendingLane, expires_at: Instant) {
        let mut state = self.state.lock().unwrap();
        if state.entries.contains_key(&nonce) {
            drop(state);
            drop(lane);
            return;
        }
        state
            .entries
            .insert(nonce, PendingLaneEntry::Ready { lane, expires_at });
        drop(state);
        self.changed.notify_one();
    }

    fn cancel_reservation(&self, nonce: PairingNonce, peer: SocketAddr, class: LaneClass) {
        let mut state = self.state.lock().unwrap();
        let should_remove = state.entries.get(&nonce).is_some_and(|entry| {
            matches!(entry, PendingLaneEntry::Building { .. })
                && entry.peer() == peer
                && entry.class() == class
        });
        if !should_remove {
            return;
        }
        let entry = state.entries.remove(&nonce).unwrap();
        drop(state);
        if let PendingLaneEntry::Building { ready, .. } = entry {
            ready.notify_one();
        }
        self.changed.notify_one();
    }

    fn next_expiry(&self) -> Option<Instant> {
        let state = self.state.lock().unwrap();
        state.entries.values().map(|e| e.expires_at()).min()
    }

    fn expire(&self, now: Instant) -> Vec<ExpiredPendingLane> {
        let mut state = self.state.lock().unwrap();
        let expired: Vec<PairingNonce> = state
            .entries
            .iter()
            .filter(|(_, e)| e.expires_at() <= now)
            .map(|(k, _)| *k)
            .collect();
        let mut removed = Vec::new();
        for nonce in &expired {
            if let Some(entry) = state.entries.remove(nonce) {
                match entry {
                    PendingLaneEntry::Building {
                        peer,
                        local_addr,
                        class,
                        ready,
                        permit,
                        ..
                    } => removed.push(ExpiredPendingLane::Building {
                        nonce: *nonce,
                        peer,
                        local_addr,
                        class,
                        ready,
                        _permit: permit,
                    }),
                    PendingLaneEntry::Ready { lane, .. } => {
                        removed.push(ExpiredPendingLane::Ready {
                            nonce: *nonce,
                            lane,
                        });
                    }
                }
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
        let rejections = LaneRejectionLog::default();
        let mut interactive_backoff = AcceptErrorBackoff::default();
        let mut bulk_backoff = AcceptErrorBackoff::default();

        {
            let registry = Arc::clone(&registry);
            let rejections = rejections.clone();
            self.mux.spawn(async move {
                run_pending_lane_expiry(registry, rejections).await;
                MuxError::TaskStopped {
                    task: "pending_lane_expiry",
                }
            });
        }

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
                _ = tokio::time::sleep(ADMISSION_REJECTION_LOG_INTERVAL) => {
                    rejections.flush();
                }
                res = self.interactive_listener.accept_without_handshake_with_mss_fec_tuning_and_frame_delivery(
                    self.fec,
                    rtp::udp::NO_FEC_MSS,
                    FecTuning::default(),
                    dual_lane_frame_delivery(),
                ) => {
                    let stream = match res {
                        Ok(res) => {
                            interactive_backoff.accepted("rtp_mux_interactive", addr);
                            res
                        }
                        Err(e) => {
                            match interactive_backoff.failed_dispatching("rtp_mux_interactive", addr, e) {
                                Ok(()) => {
                                    tokio::task::yield_now().await;
                                    continue;
                                }
                                Err(fatal) => return Err(ServeError::Accept { source: fatal, addr }),
                            }
                        }
                    };
                    counter!("stream.rtp_mux.rtp.accepts").increment(1);
                    let peer = stream.peer_addr;
                    let r = stream.read.into_async_read();
                    let w = stream.write.into_async_write();
                    let config = interactive_server_mux_config();
                    let expected_class = LaneClass::Interactive;
                    let permit = match registry.try_acquire(peer.ip()) {
                        Ok(p) => p,
                        Err(reason) => {
                            rejections.record(RejectedLaneContext {
                                class: LaneRejectionClass::Capacity,
                                peer,
                                local_addr: addr,
                                expected_class: Some(expected_class),
                                reason: reason.to_string(),
                            });
                            continue;
                        }
                    };
                    let admitted = AdmittedLane {
                        read: r, write: w, config, expected_class, peer, local_addr: addr, permit,
                    };
                    spawn_lane_accept(
                        admitted, conn_handler.clone(),
                        Arc::clone(&registry), rejections.clone(),
                    );
                }
                res = self._bulk_listener.accept_without_handshake_with_mss_fec_tuning_and_frame_delivery(
                    self.fec,
                    rtp::udp::NO_FEC_MSS,
                    FecTuning::default(),
                    dual_lane_frame_delivery(),
                ) => {
                    let stream = match res {
                        Ok(res) => {
                            bulk_backoff.accepted("rtp_mux_bulk", bulk_addr);
                            res
                        }
                        Err(e) => {
                            match bulk_backoff.failed_dispatching("rtp_mux_bulk", bulk_addr, e) {
                                Ok(()) => {
                                    tokio::task::yield_now().await;
                                    continue;
                                }
                                Err(fatal) => return Err(ServeError::Accept { source: fatal, addr: bulk_addr }),
                            }
                        }
                    };
                    counter!("stream.rtp_mux.rtp.accepts").increment(1);
                    let peer = stream.peer_addr;
                    let r = stream.read.into_async_read();
                    let w = stream.write.into_async_write();
                    let config = bulk_server_mux_config();
                    let expected_class = LaneClass::Bulk;
                    let permit = match registry.try_acquire(peer.ip()) {
                        Ok(p) => p,
                        Err(reason) => {
                            rejections.record(RejectedLaneContext {
                                class: LaneRejectionClass::Capacity,
                                peer,
                                local_addr: bulk_addr,
                                expected_class: Some(expected_class),
                                reason: reason.to_string(),
                            });
                            continue;
                        }
                    };
                    let admitted = AdmittedLane {
                        read: r, write: w, config, expected_class, peer, local_addr: bulk_addr, permit,
                    };
                    spawn_lane_accept(
                        admitted, conn_handler.clone(),
                        Arc::clone(&registry), rejections.clone(),
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
        Ok(())
    }
}

fn spawn_lane_accept(
    admitted: AdmittedLane,
    conn_handler: Arc<impl StreamServerHandleConn + Send + Sync + 'static>,
    registry: Arc<PendingLaneRegistry>,
    rejections: LaneRejectionLog,
) {
    let AdmittedLane {
        mut read,
        mut write,
        config,
        expected_class,
        peer,
        local_addr,
        permit,
    } = admitted;
    tokio::spawn(async move {
        let started = Instant::now();
        let (class, nonce) =
            match tokio::time::timeout(HELLO_DEADLINE, read_lane_hello(&mut read)).await {
                Ok(Ok(x)) => x,
                Err(_) => {
                    let elapsed = started.elapsed();
                    signal_rejected_lane(
                        &mut write,
                        &rejections,
                        RejectedLaneContext {
                            class: LaneRejectionClass::HelloTimeout,
                            peer,
                            local_addr,
                            expected_class: Some(expected_class),
                            reason: "hello deadline elapsed".to_string(),
                        },
                        elapsed,
                    )
                    .await;
                    drop(permit);
                    counter!("stream.rtp_mux.hello_timeout").increment(1);
                    return;
                }
                Ok(Err(e)) => {
                    let elapsed = started.elapsed();
                    signal_rejected_lane(
                        &mut write,
                        &rejections,
                        RejectedLaneContext {
                            class: LaneRejectionClass::HelloParse,
                            peer,
                            local_addr,
                            expected_class: Some(expected_class),
                            reason: format!("hello read/parse error: {e:?}"),
                        },
                        elapsed,
                    )
                    .await;
                    drop(permit);
                    counter!("stream.rtp_mux.hello_timeout").increment(1);
                    return;
                }
            };
        let elapsed = started.elapsed();
        if class != expected_class {
            signal_rejected_lane(
                &mut write,
                &rejections,
                RejectedLaneContext {
                    class: LaneRejectionClass::ClassMismatch,
                    peer,
                    local_addr,
                    expected_class: Some(expected_class),
                    reason: format!("lane class mismatch: got {class:?}"),
                },
                elapsed,
            )
            .await;
            drop(permit);
            counter!("stream.rtp_mux.class_mismatch").increment(1);
            return;
        }

        let mut permit_opt = Some(permit);
        let admission = registry.admit(nonce, class, peer, local_addr, &mut permit_opt);

        match admission {
            PendingLaneAdmission::Reserved => {
                match write_birth_heartbeat_result(&mut write).await {
                    Err(elapsed) => {
                        registry.cancel_reservation(nonce, peer, class);
                        signal_rejected_lane(
                            &mut write,
                            &rejections,
                            RejectedLaneContext {
                                class: LaneRejectionClass::BirthHeartbeat,
                                peer,
                                local_addr,
                                expected_class: Some(expected_class),
                                reason: "birth heartbeat failed after reservation".to_string(),
                            },
                            elapsed,
                        )
                        .await;
                        counter!("stream.rtp_mux.birth_heartbeat_error").increment(1);
                        return;
                    }
                    Ok(()) => {}
                }

                let mut lane_spawner = JoinSet::new();
                let (opener, accepter) =
                    spawn_mux_no_reconnection(read, write, config, &mut lane_spawner);
                let prepared = PreparedLane {
                    pending: mux::PendingAcceptor::new(
                        class,
                        nonce,
                        opener,
                        accepter,
                        lane_spawner,
                    ),
                    peer,
                    local_addr,
                };

                let _ = registry.finish_reservation(nonce, prepared);
            }
            PendingLaneAdmission::Wait { ready, expires_at } => {
                match write_birth_heartbeat_result(&mut write).await {
                    Err(elapsed) => {
                        signal_rejected_lane(
                            &mut write,
                            &rejections,
                            RejectedLaneContext {
                                class: LaneRejectionClass::BirthHeartbeat,
                                peer,
                                local_addr,
                                expected_class: Some(expected_class),
                                reason: "birth heartbeat failed while waiting".to_string(),
                            },
                            elapsed,
                        )
                        .await;
                        counter!("stream.rtp_mux.birth_heartbeat_error").increment(1);
                        return;
                    }
                    Ok(()) => {}
                }

                loop {
                    let notified = tokio::time::timeout_at(
                        tokio::time::Instant::from_std(expires_at),
                        ready.notified(),
                    )
                    .await;

                    match notified {
                        Err(_elapsed) => {
                            signal_rejected_lane(
                                &mut write,
                                &rejections,
                                RejectedLaneContext {
                                    class: LaneRejectionClass::PairingTimeout,
                                    peer,
                                    local_addr,
                                    expected_class: Some(expected_class),
                                    reason: "pairing deadline expired while waiting".to_string(),
                                },
                                started.elapsed(),
                            )
                            .await;
                            counter!("stream.rtp_mux.pairing_timeout").increment(1);
                            return;
                        }
                        Ok(()) => {
                            let admission =
                                registry.admit(nonce, class, peer, local_addr, &mut None);
                            match admission {
                                PendingLaneAdmission::Pair {
                                    lane: other_lane, ..
                                } => {
                                    let mut lane_spawner = JoinSet::new();
                                    let (opener, accepter) = spawn_mux_no_reconnection(
                                        read,
                                        write,
                                        config,
                                        &mut lane_spawner,
                                    );
                                    let this_lane = PendingLane {
                                        pending: mux::PendingAcceptor::new(
                                            class,
                                            nonce,
                                            opener,
                                            accepter,
                                            lane_spawner,
                                        ),
                                        peer,
                                        local_addr,
                                        _permit: permit_opt.take().unwrap(),
                                    };
                                    pair_lanes_inner(this_lane, other_lane, conn_handler, nonce);
                                    return;
                                }
                                _ => {
                                    signal_rejected_lane(
                                        &mut write,
                                        &rejections,
                                        RejectedLaneContext {
                                            class: LaneRejectionClass::ReservationLost,
                                            peer,
                                            local_addr,
                                            expected_class: Some(expected_class),
                                            reason: "reservation lost after wait notify"
                                                .to_string(),
                                        },
                                        started.elapsed(),
                                    )
                                    .await;
                                    counter!("stream.rtp_mux.pairing_timeout").increment(1);
                                    return;
                                }
                            }
                        }
                    }
                }
            }
            PendingLaneAdmission::Pair {
                lane: other_lane,
                expires_at,
            } => {
                match write_birth_heartbeat_result(&mut write).await {
                    Err(elapsed) => {
                        registry.restore_ready(nonce, other_lane, expires_at);
                        signal_rejected_lane(
                            &mut write,
                            &rejections,
                            RejectedLaneContext {
                                class: LaneRejectionClass::BirthHeartbeat,
                                peer,
                                local_addr,
                                expected_class: Some(expected_class),
                                reason: "birth heartbeat failed before pairing".to_string(),
                            },
                            elapsed,
                        )
                        .await;
                        counter!("stream.rtp_mux.birth_heartbeat_error").increment(1);
                        return;
                    }
                    Ok(()) => {}
                }

                let mut lane_spawner = JoinSet::new();
                let (opener, accepter) =
                    spawn_mux_no_reconnection(read, write, config, &mut lane_spawner);
                let this_lane = PendingLane {
                    pending: mux::PendingAcceptor::new(
                        class,
                        nonce,
                        opener,
                        accepter,
                        lane_spawner,
                    ),
                    peer,
                    local_addr,
                    _permit: permit_opt.take().unwrap(),
                };

                pair_lanes_inner(this_lane, other_lane, conn_handler, nonce);
            }
            PendingLaneAdmission::Reject(reason) => {
                signal_rejected_lane(
                    &mut write,
                    &rejections,
                    RejectedLaneContext {
                        class: LaneRejectionClass::Admission,
                        peer,
                        local_addr,
                        expected_class: Some(expected_class),
                        reason: reason.to_string(),
                    },
                    started.elapsed(),
                )
                .await;
                drop(permit_opt);
                counter!("stream.rtp_mux.pairing_timeout").increment(1);
            }
        }
    });
}

async fn write_birth_heartbeat_result(w: &mut WriteStream) -> Result<(), Duration> {
    let started = Instant::now();
    let result = tokio::time::timeout(HELLO_DEADLINE, write_birth_heartbeat(w)).await;
    match result {
        Ok(Ok(())) => Ok(()),
        Ok(Err(_)) | Err(_) => {
            let _ = w.send_kill_and_abort().await;
            Err(started.elapsed())
        }
    }
}

async fn signal_rejected_lane(
    writer: &mut WriteStream,
    rejections: &LaneRejectionLog,
    mut context: RejectedLaneContext,
    elapsed: Duration,
) {
    let _ = writer.send_kill_and_abort().await;
    context.reason = format!("{}; elapsed_ms={}", context.reason, elapsed.as_millis());
    rejections.record(context);
}

fn pair_lanes_inner(
    lane_a: PendingLane,
    lane_b: PendingLane,
    conn_handler: Arc<impl StreamServerHandleConn + Send + Sync + 'static>,
    nonce: PairingNonce,
) {
    counter!("stream.rtp_mux.paired").increment(1);
    let a_peer = lane_a.peer;
    let a_local = lane_a.local_addr;
    let a_class = lane_a.pending.class;
    let b_peer = lane_b.peer;
    let b_local = lane_b.local_addr;
    let addrs = classify_lane_addrs(a_class, a_peer, a_local, b_peer, b_local);
    pair_lanes(lane_a, lane_b, conn_handler, addrs, nonce);
}

async fn run_pending_lane_expiry(registry: Arc<PendingLaneRegistry>, rejections: LaneRejectionLog) {
    loop {
        let changed = registry.changed.notified();
        let Some(expires_at) = registry.next_expiry() else {
            changed.await;
            continue;
        };
        tokio::select! {
            () = tokio::time::sleep_until(expires_at.into()) => {
                for expired in registry.expire(Instant::now()) {
                    match expired {
                        ExpiredPendingLane::Building { nonce, peer, local_addr, class, ready, .. } => {
                            ready.notify_one();
                            rejections.record(RejectedLaneContext {
                                class: LaneRejectionClass::PairingTimeout,
                                peer,
                                local_addr,
                                expected_class: Some(class),
                                reason: format!("pairing deadline expired while preparing lane; nonce={nonce:?}"),
                            });
                        }
                        ExpiredPendingLane::Ready { nonce, lane } => {
                            rejections.record(RejectedLaneContext {
                                class: LaneRejectionClass::PairingTimeout,
                                peer: lane.peer,
                                local_addr: lane.local_addr,
                                expected_class: Some(lane.pending.class),
                                reason: format!("pairing deadline expired; nonce={nonce:?}"),
                            });
                        }
                    }
                }
                counter!("stream.rtp_mux.pairing_timeout").increment(1);
            }
            () = changed => {}
        }
    }
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
                let accepted_streams = run_dual_mux_accepter(dual_accepter, addr_pair, |stream| {
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
        let dialer: DualLaneDialer = {
            let config = Arc::clone(&config);
            Arc::new(move |addr| {
                let config = Arc::clone(&config);
                Box::pin(async move { connect_dual_lane(addr, &config, fec).await })
            })
        };
        connector.spawn(async move {
            run_dual_mux_connector_main(reset, connect_request_rx, dialer).await;
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
    dialer: DualLaneDialer,
) {
    let mut dual_openers: HashMap<SocketAddr, ConnectedDualLane> = HashMap::new();
    let mut mux_spawner: JoinSet<(SocketAddr, MuxError)> = JoinSet::new();
    let mut reset_waiter = reset.0.waiter();
    let mut pending_dials: FuturesUnordered<DualLaneDial> = FuturesUnordered::new();
    let mut in_flight_dials: HashSet<SocketAddr> = HashSet::new();
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
                in_flight_dials.clear();
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
                in_flight_dials.remove(&addr);
                match result {
                    Ok(birth) => {
                        let ConnectedDualLaneBirth { mut session, supervisor } = birth;
                        let mut sup = supervisor;
                        let sup_addr = addr;
                        mux_spawner.spawn(async move {
                            let res = sup.join_next().await;
                            (sup_addr, dual_supervisor_result(res))
                        });
                        if let Some(waiters) = dial_waiters.remove(&addr) {
                            for tx in waiters {
                                send_connected_stream(&mut session, tx);
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
                if let Some(session) = dual_openers.get_mut(&msg.listen_addr) {
                    send_connected_stream(session, msg.stream);
                    continue;
                }
                dial_waiters
                    .entry(msg.listen_addr)
                    .or_default()
                    .retain(|tx| !tx.is_closed());
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
                let is_in_flight = in_flight_dials.contains(&msg.listen_addr);
                if !is_in_flight && pending_dials.len() >= MAX_CONCURRENT_DUAL_DIALS {
                    let _ = msg.stream.send(Err(io::Error::new(
                        io::ErrorKind::WouldBlock,
                        "too many concurrent dual dials",
                    )));
                    continue;
                }
                dial_waiters
                    .entry(msg.listen_addr)
                    .or_default()
                    .push(msg.stream);
                if !is_in_flight {
                    in_flight_dials.insert(msg.listen_addr);
                    let dialer = Arc::clone(&dialer);
                    let r_addr = msg.listen_addr;
                    let dial: DualLaneDial = Box::pin(async move {
                        let result = dialer(r_addr).await.map(|birth| birth);
                        (r_addr, result)
                    });
                    pending_dials.push(dial);
                }
            }
        }
    }
}

fn send_connected_stream(
    session: &mut ConnectedDualLane,
    response: tokio::sync::oneshot::Sender<io::Result<MigratingConnStream>>,
) {
    if response.is_closed() {
        return;
    }
    let stream_id = rand::random::<u64>();
    let (writer, reader_rx) = session
        .opener
        .open_migrating_with_reader(stream_id, LaneClass::Interactive);
    session.opened_streams += 1;
    counter!("stream.rtp_mux.rtp_connects").increment(1);
    counter!("stream.rtp_mux.mux_connects").increment(1);
    let stream = MigratingConnStream::new(writer, reader_rx, session.addr_pair);
    let _ = response.send(Ok(stream));
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

    let bulk_addr = bulk_lane_addr(addr)?;

    let int_connected = rtp::udp::connect_with_mss_fec_tuning_and_frame_delivery(
        SocketAddr::new(bind_ip.ip(), bind_ip.port()),
        addr,
        rtp::udp::ConnectConfig {
            log_config: None,
            handshake: false,
            fec,
            mss: rtp::udp::NO_FEC_MSS,
            fec_tuning: FecTuning::default(),
            frame_delivery: dual_lane_frame_delivery(),
        },
    )
    .await?;
    let int_local = int_connected.local_addr;

    let bulk_bind = SocketAddr::new(bind_ip.ip(), 0);
    let bulk_connected = rtp::udp::connect_with_mss_fec_tuning_and_frame_delivery(
        bulk_bind,
        bulk_addr,
        rtp::udp::ConnectConfig {
            log_config: None,
            handshake: false,
            fec,
            mss: rtp::udp::NO_FEC_MSS,
            fec_tuning: FecTuning::default(),
            frame_delivery: dual_lane_frame_delivery(),
        },
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

    let birth_deadline = tokio::time::sleep(BIRTH_LIVENESS_DEADLINE + BIRTH_LIVENESS_GRACE);
    tokio::pin!(birth_deadline);
    tokio::select! {
        biased;
        res = dual_supervisor.join_next() => {
            let e = dual_supervisor_result(res);
            return Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                format!("dual-lane birth liveness failed: {e:?}"),
            ));
        }
        ready_result = async { tokio::try_join!(int_ready, bulk_ready) } => {
            match ready_result {
                Ok(_) => {}
                Err(_) => {
                    return Err(io::Error::new(
                        io::ErrorKind::BrokenPipe,
                        "dual-lane birth readiness channel closed",
                    ));
                }
            }
        }
        () = &mut birth_deadline => {
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
        assert!(permit.is_ok());
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
        assert!(extra.is_err());
    }

    #[test]
    fn pending_lane_registry_permit_releases_slot_on_drop() {
        let registry = PendingLaneRegistry::new();
        let ip = IpAddr::V4(Ipv4Addr::LOCALHOST);
        {
            let _permit = registry.try_acquire(ip).unwrap();
        }
        let permit2 = registry.try_acquire(ip);
        assert!(permit2.is_ok());
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
        let admission = registry.admit(nonce, LaneClass::Interactive, peer, local, &mut opt1);
        assert!(matches!(admission, PendingLaneAdmission::Reserved));
        assert!(opt1.is_none());

        let permit2 = registry.try_acquire(ip).unwrap();
        let mut opt2 = Some(permit2);
        let admission2 = registry.admit(nonce, LaneClass::Interactive, peer, local, &mut opt2);
        assert!(matches!(
            admission2,
            PendingLaneAdmission::Reject("duplicate lane class for pairing nonce")
        ));
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
        let admission = registry.admit(nonce, LaneClass::Interactive, peer1, local, &mut opt1);
        assert!(matches!(admission, PendingLaneAdmission::Reserved));

        let permit2 = registry.try_acquire(ip2).unwrap();
        let mut opt2 = Some(permit2);
        let admission2 = registry.admit(nonce, LaneClass::Bulk, peer2, local, &mut opt2);
        assert!(matches!(
            admission2,
            PendingLaneAdmission::Reject("pairing nonce peer mismatch")
        ));
        assert!(opt2.is_some());
    }

    #[test]
    fn pending_lane_registry_pairing_is_not_blocked() {
        let registry = PendingLaneRegistry::new();
        let a_addr: SocketAddr = "127.0.0.1:1000".parse().unwrap();
        let ip = a_addr.ip();
        assert!(registry.try_acquire(ip).is_ok());
    }

    #[test]
    fn lane_rejection_log_aggregates_across_classes_peers_and_lanes() {
        let log = LaneRejectionLog::default();
        let peer: SocketAddr = "127.0.0.1:1000".parse().unwrap();
        let local: SocketAddr = "127.0.0.1:2000".parse().unwrap();
        log.record(RejectedLaneContext {
            class: LaneRejectionClass::HelloTimeout,
            peer,
            local_addr: local,
            expected_class: Some(LaneClass::Interactive),
            reason: "test".to_string(),
        });
        log.record(RejectedLaneContext {
            class: LaneRejectionClass::HelloTimeout,
            peer,
            local_addr: local,
            expected_class: Some(LaneClass::Interactive),
            reason: "test".to_string(),
        });
        log.record(RejectedLaneContext {
            class: LaneRejectionClass::HelloParse,
            peer,
            local_addr: local,
            expected_class: Some(LaneClass::Bulk),
            reason: "test".to_string(),
        });
        let summary = log.inner.summary.lock().unwrap();
        assert_eq!(summary.total, 3);
        assert_eq!(
            summary.by_class.get(&LaneRejectionClass::HelloTimeout),
            Some(&2)
        );
        assert_eq!(
            summary.by_class.get(&LaneRejectionClass::HelloParse),
            Some(&1)
        );
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

        let addrs2 =
            classify_lane_addrs(LaneClass::Bulk, bulk_peer, bulk_local, int_peer, int_local);
        assert_eq!(addrs2.interactive_peer, int_peer);
        assert_eq!(addrs2.interactive_local, int_local);
        assert_eq!(addrs2.bulk_peer, bulk_peer);
        assert_eq!(addrs2.bulk_local, bulk_local);
    }

    // -------------------------------------------------------------------
    // New registry tests - PendingLaneAdmission state machine
    // -------------------------------------------------------------------

    #[test]
    fn admit_reject_duplicate_class() {
        let registry = PendingLaneRegistry::new();
        let peer: SocketAddr = "127.0.0.1:1000".parse().unwrap();
        let local: SocketAddr = "127.0.0.1:2000".parse().unwrap();
        let ip = peer.ip();
        let nonce = PairingNonce::generate();

        let permit1 = registry.try_acquire(ip).unwrap();
        let mut opt1 = Some(permit1);
        let admission = registry.admit(nonce, LaneClass::Interactive, peer, local, &mut opt1);
        assert!(matches!(admission, PendingLaneAdmission::Reserved));

        let permit2 = registry.try_acquire(ip).unwrap();
        let mut opt2 = Some(permit2);
        let admission2 = registry.admit(nonce, LaneClass::Interactive, peer, local, &mut opt2);
        assert!(matches!(
            admission2,
            PendingLaneAdmission::Reject("duplicate lane class for pairing nonce")
        ));
    }

    #[test]
    fn admit_wait_for_opposite_class_while_building() {
        let registry = PendingLaneRegistry::new();
        let peer: SocketAddr = "127.0.0.1:1000".parse().unwrap();
        let local: SocketAddr = "127.0.0.1:2000".parse().unwrap();
        let ip = peer.ip();
        let nonce = PairingNonce::generate();

        let permit1 = registry.try_acquire(ip).unwrap();
        let mut opt1 = Some(permit1);
        registry.admit(nonce, LaneClass::Interactive, peer, local, &mut opt1);

        let permit2 = registry.try_acquire(ip).unwrap();
        let mut opt2 = Some(permit2);
        let admission2 = registry.admit(nonce, LaneClass::Bulk, peer, local, &mut opt2);
        assert!(
            matches!(admission2, PendingLaneAdmission::Wait { .. }),
            "opposite class while Building should return Wait"
        );
    }

    #[test]
    fn admit_reject_duplicate_waiter() {
        let registry = PendingLaneRegistry::new();
        let peer: SocketAddr = "127.0.0.1:1000".parse().unwrap();
        let local: SocketAddr = "127.0.0.1:2000".parse().unwrap();
        let ip = peer.ip();
        let nonce = PairingNonce::generate();

        let permit1 = registry.try_acquire(ip).unwrap();
        let mut opt1 = Some(permit1);
        registry.admit(nonce, LaneClass::Interactive, peer, local, &mut opt1);

        let permit2 = registry.try_acquire(ip).unwrap();
        let mut opt2 = Some(permit2);
        let admission2 = registry.admit(nonce, LaneClass::Bulk, peer, local, &mut opt2);
        assert!(matches!(admission2, PendingLaneAdmission::Wait { .. }));

        let permit3 = registry.try_acquire(ip).unwrap();
        let mut opt3 = Some(permit3);
        let admission3 = registry.admit(nonce, LaneClass::Bulk, peer, local, &mut opt3);
        assert!(matches!(
            admission3,
            PendingLaneAdmission::Reject("opposite lane is already waiting for pairing")
        ));
    }

    #[test]
    #[should_panic(expected = "accepted RTP mux lane must retain its admission permit")]
    fn admit_reject_no_permit() {
        let registry = PendingLaneRegistry::new();
        let peer: SocketAddr = "127.0.0.1:1000".parse().unwrap();
        let local: SocketAddr = "127.0.0.1:2000".parse().unwrap();
        let nonce = PairingNonce::generate();
        let mut opt: Option<PendingLanePermit> = None;
        let _ = registry.admit(nonce, LaneClass::Interactive, peer, local, &mut opt);
    }

    #[test]
    fn admit_reject_foreign_peer() {
        let registry = PendingLaneRegistry::new();
        let peer1: SocketAddr = "127.0.0.1:1000".parse().unwrap();
        let peer2: SocketAddr = "192.168.1.1:1000".parse().unwrap();
        let local: SocketAddr = "127.0.0.1:2000".parse().unwrap();
        let ip1 = peer1.ip();
        let ip2 = peer2.ip();
        let nonce = PairingNonce::generate();

        let permit1 = registry.try_acquire(ip1).unwrap();
        let mut opt1 = Some(permit1);
        registry.admit(nonce, LaneClass::Interactive, peer1, local, &mut opt1);

        let permit2 = registry.try_acquire(ip2).unwrap();
        let mut opt2 = Some(permit2);
        let admission2 = registry.admit(nonce, LaneClass::Bulk, peer2, local, &mut opt2);
        assert!(matches!(
            admission2,
            PendingLaneAdmission::Reject("pairing nonce peer mismatch")
        ));
    }

    // -------------------------------------------------------------------
    // Ready lane tests
    // -------------------------------------------------------------------

    #[test]
    fn ready_lane_rejects_foreign_peer_and_duplicate_class() {
        let registry = PendingLaneRegistry::new();
        let peer: SocketAddr = "127.0.0.1:1000".parse().unwrap();
        let local: SocketAddr = "127.0.0.1:2000".parse().unwrap();
        let nonce = PairingNonce::generate();

        let permit = registry.try_acquire(peer.ip()).unwrap();
        let mut opt = Some(permit);
        let admission = registry.admit(nonce, LaneClass::Interactive, peer, local, &mut opt);
        assert!(matches!(admission, PendingLaneAdmission::Reserved));

        let foreign_peer: SocketAddr = "192.168.1.1:1000".parse().unwrap();
        let permit2 = registry.try_acquire(foreign_peer.ip()).unwrap();
        let mut opt2 = Some(permit2);
        let admission2 = registry.admit(nonce, LaneClass::Bulk, foreign_peer, local, &mut opt2);
        assert!(matches!(
            admission2,
            PendingLaneAdmission::Reject("pairing nonce peer mismatch")
        ));

        let peer_same: SocketAddr = "127.0.0.1:1001".parse().unwrap();
        let permit3 = registry.try_acquire(peer_same.ip()).unwrap();
        let mut opt3 = Some(permit3);
        let admission3 = registry.admit(nonce, LaneClass::Interactive, peer_same, local, &mut opt3);
        assert!(matches!(
            admission3,
            PendingLaneAdmission::Reject("duplicate lane class for pairing nonce")
        ));
    }

    #[test]
    fn ready_lane_pairs_opposite_class() {
        let registry = PendingLaneRegistry::new();
        let peer: SocketAddr = "127.0.0.1:1000".parse().unwrap();
        let local: SocketAddr = "127.0.0.1:2000".parse().unwrap();
        let nonce = PairingNonce::generate();

        let permit = registry.try_acquire(peer.ip()).unwrap();
        let mut opt = Some(permit);
        registry.admit(nonce, LaneClass::Interactive, peer, local, &mut opt);

        let permit2 = registry.try_acquire(peer.ip()).unwrap();
        let mut opt2 = Some(permit2);
        let admission2 = registry.admit(nonce, LaneClass::Bulk, peer, local, &mut opt2);
        assert!(matches!(admission2, PendingLaneAdmission::Wait { .. }));
    }

    // -------------------------------------------------------------------
    // Connector tests
    // -------------------------------------------------------------------

    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn spawn_test_connector(
        dialer: DualLaneDialer,
    ) -> (
        Arc<DualConnectRequestTx>,
        ConnectorReset,
        tokio::task::JoinHandle<()>,
    ) {
        let reset = ConnectorReset(common::notify::Notify::new());
        let (tx, rx) = dual_connect_request_channel();
        let coordinator_reset = reset.clone();
        let coordinator = tokio::spawn(async move {
            run_dual_mux_connector_main(coordinator_reset, rx, dialer).await;
        });
        (Arc::new(tx), reset, coordinator)
    }

    async fn enqueue_connector_request(
        tx: &DualConnectRequestTx,
        addr: SocketAddr,
    ) -> tokio::sync::oneshot::Receiver<io::Result<MigratingConnStream>> {
        let (response_tx, response_rx) = tokio::sync::oneshot::channel();
        tx.tx
            .send(DualConnectRequestMsg {
                listen_addr: addr,
                stream: response_tx,
            })
            .await
            .unwrap();
        response_rx
    }

    async fn stop_test_connector(
        tx: Arc<DualConnectRequestTx>,
        coordinator: tokio::task::JoinHandle<()>,
    ) {
        drop(tx);
        tokio::time::timeout(Duration::from_secs(1), coordinator)
            .await
            .expect("connector coordinator did not stop")
            .unwrap();
    }

    fn fake_connected_birth(addr: SocketAddr, terminate: Option<tokio::sync::oneshot::Receiver<()>>) -> ConnectedDualLaneBirth {
        let (interactive_local, interactive_peer) = tokio::io::duplex(64 * 1024);
        let (bulk_local, bulk_peer) = tokio::io::duplex(64 * 1024);
        let (interactive_local_read, interactive_local_write) = tokio::io::split(interactive_local);
        let (interactive_peer_read, interactive_peer_write) = tokio::io::split(interactive_peer);
        let (bulk_local_read, bulk_local_write) = tokio::io::split(bulk_local);
        let (bulk_peer_read, bulk_peer_write) = tokio::io::split(bulk_peer);
        let mut interactive_local_tasks = JoinSet::new();
        let (interactive_opener, interactive_accepter) = spawn_mux_no_reconnection(interactive_local_read, interactive_local_write, interactive_client_mux_config(), &mut interactive_local_tasks);
        let mut bulk_local_tasks = JoinSet::new();
        let (bulk_opener, bulk_accepter) = spawn_mux_no_reconnection(bulk_local_read, bulk_local_write, bulk_client_mux_config(), &mut bulk_local_tasks);
        let mut interactive_peer_tasks = JoinSet::new();
        let (interactive_peer_opener, interactive_peer_accepter) = spawn_mux_no_reconnection(interactive_peer_read, interactive_peer_write, interactive_server_mux_config(), &mut interactive_peer_tasks);
        let mut bulk_peer_tasks = JoinSet::new();
        let (bulk_peer_opener, bulk_peer_accepter) = spawn_mux_no_reconnection(bulk_peer_read, bulk_peer_write, bulk_server_mux_config(), &mut bulk_peer_tasks);
        let mut supervisor = JoinSet::new();
        let (opener, _accepter) = mux::spawn_dual_mux_paired_supervised(interactive_opener, interactive_accepter, interactive_local_tasks, bulk_opener, bulk_accepter, bulk_local_tasks, &mut supervisor);
        supervisor.spawn(async move {
            let _keep_peer_alive = (interactive_peer_opener, interactive_peer_accepter, interactive_peer_tasks, bulk_peer_opener, bulk_peer_accepter, bulk_peer_tasks);
            std::future::pending::<MuxError>().await
        });
        if let Some(terminate) = terminate {
            supervisor.spawn(async move {
                let _ = terminate.await;
                MuxError::TaskStopped { task: "synthetic_dual_lane" }
            });
        }
        ConnectedDualLaneBirth {
            session: ConnectedDualLane {
                opener,
                addr_pair: SocketAddrPair {
                    local_addr: "192.0.2.100:40000".parse().unwrap(),
                    peer_addr: addr,
                },
                nonce: PairingNonce::generate(),
                connected_at: Instant::now(),
                opened_streams: 0,
            },
            supervisor,
        }
    }

    #[tokio::test] async fn pending_dial_does_not_block_other_destinations() {
        let blocked_addr: SocketAddr = "192.0.2.1:50000".parse().unwrap();
        let fast_addr: SocketAddr = "192.0.2.2:50000".parse().unwrap();
        let blocked_started = Arc::new(tokio::sync::Notify::new());
        let dialer: DualLaneDialer = Arc::new({
            let blocked_started = Arc::clone(&blocked_started);
            move |addr| {
                if addr == blocked_addr {
                    blocked_started.notify_one();
                    Box::pin(std::future::pending())
                } else {
                    Box::pin(async { Err(io::Error::new(io::ErrorKind::ConnectionRefused, "synthetic dial failure")) })
                }
            }
        });
        let (tx, _reset, coordinator) = spawn_test_connector(dialer);
        let blocked_request = tokio::spawn({
            let tx = Arc::clone(&tx);
            async move { tx.send(blocked_addr).await }
        });
        blocked_started.notified().await;
        let error = tokio::time::timeout(Duration::from_secs(1), tx.send(fast_addr))
            .await
            .expect("second destination was blocked by the first dial")
            .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::ConnectionRefused);
        blocked_request.abort();
        blocked_request.await.unwrap_err();
        stop_test_connector(tx, coordinator).await;
    }

    #[tokio::test] async fn pending_dial_does_not_block_cached_session_requests() {
        let cached_addr: SocketAddr = "192.0.2.1:50000".parse().unwrap();
        let blocked_addr: SocketAddr = "192.0.2.2:50000".parse().unwrap();
        let blocked_started = Arc::new(tokio::sync::Notify::new());
        let dialer: DualLaneDialer = Arc::new({
            let blocked_started = Arc::clone(&blocked_started);
            move |addr| {
                if addr == cached_addr {
                    Box::pin(async move { Ok(fake_connected_birth(addr, None)) })
                } else {
                    blocked_started.notify_one();
                    Box::pin(std::future::pending())
                }
            }
        });
        let (tx, reset, coordinator) = spawn_test_connector(dialer);
        drop(tx.send(cached_addr).await.unwrap());
        let blocked = enqueue_connector_request(&tx, blocked_addr).await;
        blocked_started.notified().await;
        let cached = tokio::time::timeout(Duration::from_secs(1), tx.send(cached_addr))
            .await
            .expect("pending dial blocked a cached session request")
            .unwrap();
        drop(cached);
        reset.0.notify_waiters();
        assert_eq!(blocked.await.unwrap().unwrap_err().kind(), io::ErrorKind::ConnectionAborted);
        stop_test_connector(tx, coordinator).await;
    }

    #[tokio::test] async fn pending_dial_does_not_block_dead_session_reaping() {
        let session_addr: SocketAddr = "192.0.2.1:50000".parse().unwrap();
        let blocked_addr: SocketAddr = "192.0.2.2:50000".parse().unwrap();
        let (terminate_tx, terminate_rx) = tokio::sync::oneshot::channel();
        let terminate_rx = Arc::new(Mutex::new(Some(terminate_rx)));
        let attempts = Arc::new(AtomicUsize::new(0));
        let blocked_started = Arc::new(tokio::sync::Notify::new());
        let dialer: DualLaneDialer = Arc::new({
            let terminate_rx = Arc::clone(&terminate_rx);
            let attempts = Arc::clone(&attempts);
            let blocked_started = Arc::clone(&blocked_started);
            move |addr| {
                if addr == blocked_addr {
                    blocked_started.notify_one();
                    return Box::pin(std::future::pending());
                }
                if attempts.fetch_add(1, Ordering::SeqCst) == 0 {
                    let terminate = terminate_rx.lock().unwrap().take().unwrap();
                    Box::pin(async move { Ok(fake_connected_birth(addr, Some(terminate))) })
                } else {
                    Box::pin(async { Err(io::Error::new(io::ErrorKind::ConnectionRefused, "synthetic redial failure")) })
                }
            }
        });
        let (tx, reset, coordinator) = spawn_test_connector(dialer);
        drop(tx.send(session_addr).await.unwrap());
        let blocked = enqueue_connector_request(&tx, blocked_addr).await;
        blocked_started.notified().await;
        terminate_tx.send(()).unwrap();
        let error = tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                match tx.send(session_addr).await {
                    Ok(stream) => {
                        drop(stream);
                        tokio::task::yield_now().await;
                    }
                    Err(error) => break error,
                }
            }
        })
        .await
        .expect("pending dial blocked dead-session reaping");
        assert_eq!(error.kind(), io::ErrorKind::ConnectionRefused);
        assert_eq!(attempts.load(Ordering::SeqCst), 2);
        reset.0.notify_waiters();
        assert_eq!(blocked.await.unwrap().unwrap_err().kind(), io::ErrorKind::ConnectionAborted);
        stop_test_connector(tx, coordinator).await;
    }

    #[tokio::test] async fn connector_enforces_concurrent_dial_capacity_at_boundary() {
        let dial_count = Arc::new(AtomicUsize::new(0));
        let dialer: DualLaneDialer = Arc::new({
            let dial_count = Arc::clone(&dial_count);
            move |_addr| {
                dial_count.fetch_add(1, Ordering::SeqCst);
                Box::pin(std::future::pending())
            }
        });
        let (tx, reset, coordinator) = spawn_test_connector(dialer);
        let mut responses = Vec::new();
        for port in 10_000..10_000 + MAX_CONCURRENT_DUAL_DIALS as u16 {
            responses.push(enqueue_connector_request(&tx, SocketAddr::from(([192, 0, 2, 1], port))).await);
        }
        tokio::time::timeout(Duration::from_secs(1), async {
            while dial_count.load(Ordering::SeqCst) != MAX_CONCURRENT_DUAL_DIALS {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("coordinator did not start all admitted dials");
        let rejected = enqueue_connector_request(&tx, SocketAddr::from(([192, 0, 2, 1], 20_000))).await;
        assert_eq!(rejected.await.unwrap().unwrap_err().kind(), io::ErrorKind::WouldBlock);
        reset.0.notify_waiters();
        for response in responses {
            assert_eq!(response.await.unwrap().unwrap_err().kind(), io::ErrorKind::ConnectionAborted);
        }
        stop_test_connector(tx, coordinator).await;
    }

    #[tokio::test] async fn connector_enforces_waiter_capacity_and_reset_fails_every_waiter() {
        let addr: SocketAddr = "192.0.2.1:50000".parse().unwrap();
        let dialer: DualLaneDialer = Arc::new(|_addr| Box::pin(std::future::pending()));
        let (tx, reset, coordinator) = spawn_test_connector(dialer);
        let mut responses = Vec::new();
        for _ in 0..MAX_DIAL_WAITERS_PER_ADDR {
            responses.push(enqueue_connector_request(&tx, addr).await);
        }
        let rejected = enqueue_connector_request(&tx, addr).await;
        assert_eq!(rejected.await.unwrap().unwrap_err().kind(), io::ErrorKind::WouldBlock);
        reset.0.notify_waiters();
        for response in responses {
            assert_eq!(response.await.unwrap().unwrap_err().kind(), io::ErrorKind::ConnectionAborted);
        }
        stop_test_connector(tx, coordinator).await;
    }

    #[tokio::test] async fn closed_waiters_do_not_consume_per_destination_capacity() {
        let addr: SocketAddr = "192.0.2.1:50000".parse().unwrap();
        let barrier_addr: SocketAddr = "192.0.2.2:50000".parse().unwrap();
        let dialer: DualLaneDialer = Arc::new(move |dial_addr| {
            if dial_addr == addr {
                Box::pin(std::future::pending())
            } else {
                Box::pin(async { Err(io::Error::new(io::ErrorKind::ConnectionRefused, "synthetic barrier failure")) })
            }
        });
        let (tx, reset, coordinator) = spawn_test_connector(dialer);
        let first = enqueue_connector_request(&tx, addr).await;
        for _ in 0..MAX_DIAL_WAITERS_PER_ADDR * 2 {
            drop(enqueue_connector_request(&tx, addr).await);
        }
        let live = enqueue_connector_request(&tx, addr).await;
        let barrier = enqueue_connector_request(&tx, barrier_addr).await;
        assert_eq!(barrier.await.unwrap().unwrap_err().kind(), io::ErrorKind::ConnectionRefused);
        reset.0.notify_waiters();
        assert_eq!(first.await.unwrap().unwrap_err().kind(), io::ErrorKind::ConnectionAborted);
        assert_eq!(live.await.unwrap().unwrap_err().kind(), io::ErrorKind::ConnectionAborted);
        stop_test_connector(tx, coordinator).await;
    }

    #[tokio::test] async fn connector_can_redial_immediately_after_reset() {
        let addr: SocketAddr = "192.0.2.1:50000".parse().unwrap();
        let attempts = Arc::new(AtomicUsize::new(0));
        let first_started = Arc::new(tokio::sync::Notify::new());
        let dialer: DualLaneDialer = Arc::new({
            let attempts = Arc::clone(&attempts);
            let first_started = Arc::clone(&first_started);
            move |_addr| {
                if attempts.fetch_add(1, Ordering::SeqCst) == 0 {
                    first_started.notify_one();
                    Box::pin(std::future::pending())
                } else {
                    Box::pin(async { Err(io::Error::new(io::ErrorKind::ConnectionRefused, "synthetic redial failure")) })
                }
            }
        });
        let (tx, reset, coordinator) = spawn_test_connector(dialer);
        let first = enqueue_connector_request(&tx, addr).await;
        first_started.notified().await;
        reset.0.notify_waiters();
        assert_eq!(first.await.unwrap().unwrap_err().kind(), io::ErrorKind::ConnectionAborted);
        let error = tokio::time::timeout(Duration::from_secs(1), tx.send(addr))
            .await
            .expect("redial was not admitted after reset")
            .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::ConnectionRefused);
        assert_eq!(attempts.load(Ordering::SeqCst), 2);
        stop_test_connector(tx, coordinator).await;
    }
}
