use std::{
    collections::HashMap,
    io,
    net::SocketAddr,
    sync::{Arc, Mutex, RwLock},
    time::Duration,
};

use async_trait::async_trait;
use metrics::counter;
use mux::{
    LaneClass, MuxError, PairingNonce, complete_pairing, read_lane_hello,
    spawn_mux_no_reconnection,
};
use serde::Deserialize;
use thiserror::Error;
use tokio::{net::ToSocketAddrs, task::JoinSet};
use tracing::{info, instrument, trace, warn};

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
    MigratingConnStream, SocketAddrPair, bulk_client_mux_config, interactive_client_mux_config,
    interactive_server_mux_config, run_dual_mux_accepter,
};

use rtp::{
    socket::{ReadStream, WriteStream},
    transmission::{fec_tuning::FecTuning, frame_delivery::FrameDelivery},
};

const PAIRING_DEADLINE: Duration = Duration::from_secs(10);
const HELLO_DEADLINE: Duration = Duration::from_secs(5);

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
    #[instrument(skip(self))]
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
        let pending: Arc<Mutex<HashMap<PairingNonce, mux::PendingAcceptor>>> =
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
                    FrameDelivery::enabled(),
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
                        r, w, config, expected_class, peer, addr,
                        pending.clone(),
                        conn_handler.clone(),
                    );
                }
                // Bulk lane accept (stock rtp — explicit FrameDelivery::default())
                res = self._bulk_listener.accept_without_handshake_with_mss_fec_tuning_and_frame_delivery(
                    self.fec,
                    rtp::udp::NO_FEC_MSS,
                    FecTuning::default(),
                    FrameDelivery::default(),
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
                    let config = mux::MuxConfig {
                        initiation: mux::Initiation::Server,
                        heartbeat_interval: Duration::from_secs(5),
                        frame_reassembly: false,
                    };
                    let expected_class = LaneClass::Bulk;
                    spawn_lane_accept(
                        r, w, config, expected_class, peer, bulk_addr,
                        pending.clone(),
                        conn_handler.clone(),
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
    expected_class: LaneClass,
    peer: SocketAddr,
    local_addr: SocketAddr,
    pending: Arc<Mutex<HashMap<PairingNonce, mux::PendingAcceptor>>>,
    conn_handler: Arc<impl StreamServerHandleConn + Send + Sync + 'static>,
) {
    tokio::spawn(async move {
        // Read lane hello manually — if it fails we send a kill-and-abort
        // on the raw transport so the RTP transmission layer is released
        // at once rather than lingering through a graceful FIN handshake
        // (S8: prompt abort for rejected lanes).
        let (class, nonce) =
            match tokio::time::timeout(HELLO_DEADLINE, read_lane_hello(&mut r)).await {
                Ok(Ok(x)) => x,
                Ok(Err(e)) => {
                    warn!(?e, ?peer, "Lane hello read error");
                    counter!("stream.rtp_mux.hello_timeout").increment(1);
                    w.send_kill_and_abort().await;
                    return;
                }
                Err(_) => {
                    warn!(?peer, "Lane hello deadline elapsed");
                    counter!("stream.rtp_mux.hello_timeout").increment(1);
                    w.send_kill_and_abort().await;
                    return;
                }
            };

        if class != expected_class {
            warn!(?peer, ?class, ?expected_class, "Lane class mismatch");
            counter!("stream.rtp_mux.class_mismatch").increment(1);
            w.send_kill_and_abort().await;
            return;
        }

        // Spawn mux session
        let mut lane_spawner = JoinSet::new();
        let (opener, accepter) = spawn_mux_no_reconnection(r, w, config, &mut lane_spawner);

        let lane_pending = mux::PendingAcceptor {
            class,
            nonce,
            opener,
            accepter,
            spawner: lane_spawner,
        };

        // Check for existing pending lane.  remove + insert must be a single
        // critical section so two same-nonce lanes cannot race, both see None,
        // and silently overwrite each other's PendingAcceptor.
        let mut guard = pending.lock().unwrap();
        let existing = guard.remove(&nonce);

        if let Some(other) = existing {
            drop(guard);
            counter!("stream.rtp_mux.paired").increment(1);
            let mut local_mux = JoinSet::new();
            match complete_pairing(other, lane_pending, &mut local_mux) {
                Ok((_dual_opener, dual_accepter)) => {
                    let addr_pair = SocketAddrPair {
                        local_addr,
                        peer_addr: peer,
                    };
                    tokio::spawn(async move {
                        let _mux = local_mux;
                        run_dual_mux_accepter(dual_accepter, addr_pair, |stream| {
                            counter!("stream.rtp_mux.mux.accepts").increment(1);
                            let conn_handler = Arc::clone(&conn_handler);
                            tokio::spawn(async move {
                                conn_handler.handle_stream(stream).await;
                            });
                        })
                        .await;
                    });
                }
                Err(e) => {
                    warn!(?e, "Pairing failed");
                    counter!("stream.rtp_mux.pairing_timeout").increment(1);
                }
            }
        } else {
            // Store for later pairing. Schedule expiry.
            guard.insert(nonce, lane_pending);
            drop(guard);
            let nonce_for_expiry = nonce;
            let pending_for_expiry = pending.clone();
            tokio::spawn(async move {
                tokio::time::sleep(PAIRING_DEADLINE).await;
                if pending_for_expiry
                    .lock()
                    .unwrap()
                    .remove(&nonce_for_expiry)
                    .is_some()
                {
                    warn!(?peer, ?nonce_for_expiry, "Pairing deadline expired");
                    counter!("stream.rtp_mux.pairing_timeout").increment(1);
                }
            });
        }
    });
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

    let mut dual_openers: HashMap<SocketAddr, (mux::DualStreamOpener, SocketAddrPair)> =
        HashMap::new();
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
                        warn!(?e, ?addr, "Dual-lane MUX error");
                        dual_openers.remove(&addr);
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
                        Ok((opener, addr_pair)) => { e.insert((opener, addr_pair)); }
                        Err(err) => {
                            let _ = msg.stream.send(Err(err));
                            continue;
                        }
                    }
                }
                let (opener, addr_pair) = dual_openers.get(&msg.listen_addr).unwrap();
                let stream_id = rand::random::<u64>();
                let (writer, reader_rx) = opener.open_migrating_with_reader(
                    stream_id,
                    LaneClass::Interactive,
                );

                counter!("stream.rtp_mux.rtp.connects").increment(1);
                counter!("stream.rtp_mux.mux.connects").increment(1);

                let stream = MigratingConnStream::new(writer, reader_rx, *addr_pair);
                let _ = msg.stream.send(Ok(stream));
            }
        }
    }
}

async fn connect_dual_lane(
    addr: SocketAddr,
    config: &Arc<RwLock<ConnectorConfig>>,
    fec: bool,
    mux_spawner: &mut JoinSet<(SocketAddr, MuxError)>,
) -> io::Result<(mux::DualStreamOpener, SocketAddrPair)> {
    let bind_ip = config
        .read()
        .unwrap()
        .bind
        .get_matched(&addr.ip())
        .map(|ip| SocketAddr::new(ip, 0))
        .unwrap_or_else(|| any_addr(&addr.ip()));

    // Interactive lane: frame-delivery rtp
    let int_connected = rtp::udp::connect_with_mss_fec_tuning_and_frame_delivery(
        SocketAddr::new(bind_ip.ip(), bind_ip.port()),
        addr,
        None,
        false,
        fec,
        rtp::udp::NO_FEC_MSS,
        FecTuning::default(),
        FrameDelivery::enabled(),
    )
    .await?;
    let int_local = int_connected.local_addr;

    // Bulk lane: stock rtp, port+1 — explicit FrameDelivery::default() so
    // an asymmetric RTP_FRAME_DELIVERY env between client and server cannot
    // wedge the bulk lane with mismatched framing.
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
        FrameDelivery::default(),
    )
    .await?;

    // Write lane hellos
    let nonce = PairingNonce::generate();
    let int_r = int_connected.read.into_async_read();
    let mut int_w = int_connected.write.into_async_write();
    let bulk_r = bulk_connected.read.into_async_read();
    let mut bulk_w = bulk_connected.write.into_async_write();

    if let Err(e) = mux::write_lane_hello(&mut int_w, LaneClass::Interactive, nonce).await {
        return Err(io::Error::other(format!("interactive lane hello: {e:?}")));
    }
    if let Err(e) = mux::write_lane_hello(&mut bulk_w, LaneClass::Bulk, nonce).await {
        return Err(io::Error::other(format!("bulk lane hello: {e:?}")));
    }

    // Spawn mux per lane with per-lane configs
    let int_config = interactive_client_mux_config();
    let bulk_config = bulk_client_mux_config();

    let mut int_spawner = JoinSet::new();
    let (int_opener, int_accepter) =
        mux::spawn_mux_no_reconnection(int_r, int_w, int_config, &mut int_spawner);
    let mut bulk_spawner = JoinSet::new();
    let (bulk_opener, bulk_accepter) =
        mux::spawn_mux_no_reconnection(bulk_r, bulk_w, bulk_config, &mut bulk_spawner);

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

    // Birth verification: if either lane's mux session dies within the
    // grace period, fail the connect so the caller can retry rather
    // than receiving a dead opener whose first write fails.  The biased
    // select ensures the death branch wins when both the supervisor
    // finished and the grace sleep elapsed — without bias the opener
    // could be returned dead, bypassing birth-retry.
    const BIRTH_GRACE: Duration = Duration::from_millis(200);
    tokio::select! {
        biased;
        result = dual_supervisor.join_next() => {
            return Err(io::Error::other(format!(
                "dual-lane birth failed: {:?}", result
            )));
        }
        _ = tokio::time::sleep(BIRTH_GRACE) => {
            // Grace period elapsed; lanes are alive
        }
    }

    let addr_key = addr;
    mux_spawner.spawn(async move {
        let e = match dual_supervisor.join_next().await {
            Some(Ok(e)) => e,
            Some(Err(e)) if e.is_cancelled() => MuxError::TaskJoin {
                task: "dual_lane",
                source: e,
            },
            Some(Err(e)) => MuxError::TaskJoin {
                task: "dual_lane",
                source: e,
            },
            None => MuxError::TaskStopped { task: "dual_lane" },
        };
        (addr_key, e)
    });

    let addr_pair = SocketAddrPair {
        local_addr: int_local,
        peer_addr: addr,
    };
    Ok((dual_opener, addr_pair))
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
    /// No `frame_delivery` config knob — the interactive lane always uses
    /// frame-delivery rtp; the bulk lane always uses stock rtp.
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
