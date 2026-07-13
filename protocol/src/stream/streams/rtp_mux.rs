use std::{
    collections::HashMap,
    io,
    net::SocketAddr,
    sync::{Arc, Mutex, RwLock},
    time::{Duration, Instant},
};

use async_trait::async_trait;
use metrics::counter;
use mux::{
    LaneClass, MuxError, PairingNonce, complete_pairing, read_lane_hello,
    spawn_mux_no_reconnection, spawn_mux_no_reconnection_with_first_receive_deadline,
    write_birth_heartbeat,
};
use serde::Deserialize;
use thiserror::Error;
use tokio::{net::ToSocketAddrs, task::JoinSet};
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

use crate::stream::streams::mux::{
    MigratingConnStream, SocketAddrPair, bulk_client_mux_config, bulk_server_mux_config,
    interactive_client_mux_config, interactive_server_mux_config, run_dual_mux_accepter,
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

const fn dual_lane_frame_delivery() -> FrameDelivery {
    FrameDelivery::enabled()
}

struct PendingLane {
    pending: mux::PendingAcceptor,
    peer: SocketAddr,
    local_addr: SocketAddr,
}

struct ConnectedDualLane {
    opener: mux::DualStreamOpener,
    addr_pair: SocketAddrPair,
    nonce: PairingNonce,
    connected_at: Instant,
    opened_streams: u64,
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
        let pending: Arc<Mutex<HashMap<PairingNonce, PendingLane>>> =
            Arc::new(Mutex::new(HashMap::new()));

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
                // Interactive lane accept (frame-delivery rtp)
                res = self.interactive_listener.accept_without_handshake_with_mss_fec_tuning_and_frame_delivery(
                    self.fec,
                    rtp::udp::NO_FEC_MSS,
                    FecTuning::default(),
                    dual_lane_frame_delivery(),
                ) => {
                    let stream = match res {
                        Ok(res) => res,
                        Err(e) => {
                            warn!(?e, ?addr, "Interactive RTP accept error");
                            continue;
                        }
                    };
                    counter!("stream.rtp_mux.rtp.accepts").increment(1);
                    let peer = stream.peer_addr;
                    let r = stream.read.into_async_read();
                    let w = stream.write.into_async_write();
                    let config = interactive_server_mux_config();
                    let expected_class = LaneClass::Interactive;
                    spawn_lane_accept(
                        r, w, config, conn_handler.clone(), expected_class, peer, addr,
                        pending.clone(),
                    );
                }
                // Bulk lane accept (stock rtp — explicit FrameDelivery::default())
                res = self._bulk_listener.accept_without_handshake_with_mss_fec_tuning_and_frame_delivery(
                    self.fec,
                    rtp::udp::NO_FEC_MSS,
                    FecTuning::default(),
                    dual_lane_frame_delivery(),
                ) => {
                    let stream = match res {
                        Ok(res) => res,
                        Err(e) => {
                            warn!(?e, ?bulk_addr, "Bulk RTP accept error");
                            continue;
                        }
                    };
                    counter!("stream.rtp_mux.rtp.accepts").increment(1);
                    let peer = stream.peer_addr;
                    let r = stream.read.into_async_read();
                    let w = stream.write.into_async_write();
                    let config = bulk_server_mux_config();
                    let expected_class = LaneClass::Bulk;
                    spawn_lane_accept(
                        r, w, config, conn_handler.clone(), expected_class, peer, bulk_addr,
                        pending.clone(),
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
    mut r: ReadStream,
    mut w: WriteStream,
    config: mux::MuxConfig,
    conn_handler: Arc<impl StreamServerHandleConn + Send + Sync + 'static>,
    expected_class: LaneClass,
    peer: SocketAddr,
    local_addr: SocketAddr,
    pending: Arc<Mutex<HashMap<PairingNonce, PendingLane>>>,
) {
    tokio::spawn(async move {
        let started = Instant::now();
        let (class, nonce) = match tokio::time::timeout(HELLO_DEADLINE, read_lane_hello(&mut r))
            .await
        {
            Ok(Ok(x)) => x,
            Err(_) => {
                let elapsed = started.elapsed();
                let reason = "hello deadline elapsed".to_string();
                signal_rejected_lane(&mut w, peer, local_addr, expected_class, elapsed, &reason)
                    .await;
                counter!("stream.rtp_mux.hello_timeout").increment(1);
                return;
            }
            Ok(Err(e)) => {
                let elapsed = started.elapsed();
                let reason = format!("hello read/parse error: {e:?}");
                signal_rejected_lane(&mut w, peer, local_addr, expected_class, elapsed, &reason)
                    .await;
                counter!("stream.rtp_mux.hello_timeout").increment(1);
                return;
            }
        };
        let elapsed = started.elapsed();
        if class != expected_class {
            let reason = format!("lane class mismatch: got {class:?}");
            signal_rejected_lane(&mut w, peer, local_addr, expected_class, elapsed, &reason).await;
            counter!("stream.rtp_mux.class_mismatch").increment(1);
            return;
        }
        if let Err(e) = write_birth_heartbeat(&mut w).await {
            let kill_result = w.send_kill_and_abort().await;
            warn!(
                ?e,
                ?kill_result,
                dn = ?peer,
                dn_local = ?local_addr,
                ?expected_class,
                "Failed to write RTP mux birth heartbeat; killed lane"
            );
            counter!("stream.rtp_mux.birth_heartbeat_error").increment(1);
            return;
        }
        let mut lane_spawner = JoinSet::new();
        let (opener, accepter) = spawn_mux_no_reconnection(r, w, config, &mut lane_spawner);
        let lane_pending = PendingLane {
            pending: mux::PendingAcceptor {
                class,
                nonce,
                opener,
                accepter,
                spawner: lane_spawner,
            },
            peer,
            local_addr,
        };
        let existing = {
            let mut guard = pending.lock().unwrap();
            match guard.entry(nonce) {
                std::collections::hash_map::Entry::Occupied(entry) => {
                    Some((entry.remove(), lane_pending))
                }
                std::collections::hash_map::Entry::Vacant(entry) => {
                    entry.insert(lane_pending);
                    None
                }
            }
        };
        if let Some((other, lane_pending)) = existing {
            counter!("stream.rtp_mux.paired").increment(1);
            let other_peer = other.peer;
            let other_local_addr = other.local_addr;
            let other_class = other.pending.class;
            let current_peer = lane_pending.peer;
            let current_local_addr = lane_pending.local_addr;
            let current_class = lane_pending.pending.class;
            let mut local_mux = JoinSet::new();
            match complete_pairing(other.pending, lane_pending.pending, &mut local_mux) {
                Ok((_dual_opener, dual_accepter)) => {
                    let (interactive_dn, interactive_local, bulk_dn, bulk_local) = match other_class
                    {
                        LaneClass::Interactive => (
                            other_peer,
                            other_local_addr,
                            current_peer,
                            current_local_addr,
                        ),
                        LaneClass::Bulk => (
                            current_peer,
                            current_local_addr,
                            other_peer,
                            other_local_addr,
                        ),
                    };
                    let addr_pair = SocketAddrPair {
                        local_addr,
                        peer_addr: peer,
                    };
                    let conn_handler2 = Arc::clone(&conn_handler);
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
                        dn_other = ?other_peer,
                        dn_other_local = ?other_local_addr,
                        ?other_class,
                        dn_current = ?current_peer,
                        dn_current_local = ?current_local_addr,
                        ?current_class,
                        "RTP mux dual-lane pairing failed"
                    );
                    counter!("stream.rtp_mux.pairing_timeout").increment(1);
                }
            }
        } else {
            let nonce_for_expiry = nonce;
            let pending_for_expiry = pending.clone();
            tokio::spawn(async move {
                tokio::time::sleep(PAIRING_DEADLINE).await;
                if let Some(lane) = pending_for_expiry.lock().unwrap().remove(&nonce_for_expiry) {
                    warn!(
                        dn = ?lane.peer,
                        dn_local = ?lane.local_addr,
                        class = ?lane.pending.class,
                        ?nonce_for_expiry,
                        "Pairing deadline expired"
                    );
                    counter!("stream.rtp_mux.pairing_timeout").increment(1);
                }
            });
        }
    });
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
}

// ---------------------------------------------------------------------------
// RtpMuxConnector — dual-lane connect
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
    use std::collections::{HashMap, hash_map};

    let mut dual_openers: HashMap<SocketAddr, ConnectedDualLane> = HashMap::new();
    let mut mux_spawner: JoinSet<(SocketAddr, MuxError)> = JoinSet::new();
    let mut reset_notified = reset.0.waiter();

    loop {
        tokio::select! {
            () = reset_notified.notified() => {
                dual_openers.clear();
                mux_spawner = JoinSet::new();
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
            res = connect_request_rx.recv() => {
                let Some(msg) = res else {
                    break;
                };
                if let hash_map::Entry::Vacant(e) = dual_openers.entry(msg.listen_addr) {
                    match connect_dual_lane(msg.listen_addr, &config, fec, &mut mux_spawner).await {
                        Ok(session) => { e.insert(session); }
                        Err(err) => {
                            let _ = msg.stream.send(Err(err));
                            continue;
                        }
                    }
                }
                let session = dual_openers.get_mut(&msg.listen_addr).unwrap();
                let stream_id = rand::random::<u64>();
                let (writer, reader_rx) = session.opener.open_migrating_with_reader(
                    stream_id,
                    LaneClass::Interactive,
                );

                session.opened_streams += 1;
                counter!("stream.rtp_mux.rtp_connects").increment(1);
                counter!("stream.rtp_mux.mux_connects").increment(1);

                let stream = MigratingConnStream::new(writer, reader_rx, session.addr_pair);
                let _ = msg.stream.send(Ok(stream));
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
    mux_spawner: &mut JoinSet<(SocketAddr, MuxError)>,
) -> io::Result<ConnectedDualLane> {
    let started = Instant::now();
    let mut failures = Vec::new();
    for attempt in 1..=MAX_DUAL_CONNECT_ATTEMPTS {
        let attempt_started = Instant::now();
        match connect_dual_lane_once(addr, config, fec, mux_spawner).await {
            Ok(session) => {
                if attempt > 1 {
                    info!(
                        ?addr,
                        attempt,
                        failures = %failures.join("; "),
                        elapsed_ms = started.elapsed().as_millis(),
                        "RTP mux dual-lane birth recovered after retry"
                    );
                }
                return Ok(session);
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
    mux_spawner: &mut JoinSet<(SocketAddr, MuxError)>,
) -> io::Result<ConnectedDualLane> {
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

    let bulk_addr = SocketAddr::new(addr.ip(), addr.port().wrapping_add(1));
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
    let (int_opener, int_accepter) = spawn_mux_no_reconnection_with_first_receive_deadline(
        int_r,
        int_w,
        int_config,
        BIRTH_LIVENESS_DEADLINE,
        &mut int_spawner,
    );
    let mut bulk_spawner = JoinSet::new();
    let (bulk_opener, bulk_accepter) = spawn_mux_no_reconnection_with_first_receive_deadline(
        bulk_r,
        bulk_w,
        bulk_config,
        BIRTH_LIVENESS_DEADLINE,
        &mut bulk_spawner,
    );

    let mut dual_supervisor = JoinSet::new();
    let (dual_opener, _dual_accepter) = mux::spawn_dual_mux_paired_supervised(
        int_opener,
        int_accepter,
        int_spawner,
        bulk_opener,
        bulk_accepter,
        bulk_spawner,
        &mut dual_supervisor,
    );

    let birth_wait = BIRTH_LIVENESS_DEADLINE + BIRTH_LIVENESS_GRACE;
    tokio::select! {
        biased;
        res = dual_supervisor.join_next() => {
            let e = dual_supervisor_result(res);
            return Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                format!("dual-lane birth liveness failed: {e:?}"),
            ));
        }
        () = tokio::time::sleep(birth_wait) => {}
    }

    let addr_key = addr;
    mux_spawner.spawn(async move {
        let e = dual_supervisor_result(dual_supervisor.join_next().await);
        (addr_key, e)
    });

    let addr_pair = SocketAddrPair {
        local_addr: int_local,
        peer_addr: addr,
    };
    Ok(ConnectedDualLane {
        opener: dual_opener,
        addr_pair,
        nonce,
        connected_at: Instant::now(),
        opened_streams: 0,
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
    /// Address to listen on for the **interactive** lane.
    ///
    /// The **bulk** lane is bound to `listen_addr` port + 1.  Both ports
    /// must be free at startup; `port+1` is never configurable — the peer
    /// MUST connect its bulk lane to `addr.port + 1`.  This is a flag-day
    /// change: old single-connection peers cannot interoperate.
    ///
    /// No `frame_delivery` config knob — both lanes always use frame-delivery
    /// RTP so either lane's heartbeat can bypass
    /// an unrelated packet gap.
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
    let bulk_addr = SocketAddr::new(int_addr.ip(), int_addr.port().wrapping_add(1));
    let bulk_listener = rtp::udp::Listener::bind(bulk_addr)
        .await
        .map_err(ListenerBindError)?;
    let server = RtpMuxServer::new(interactive_listener, bulk_listener, stream_proxy, fec);
    Ok(server)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn both_rtp_lanes_enable_frame_delivery() {
        assert!(dual_lane_frame_delivery().enabled);
    }
}
