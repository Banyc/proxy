//! The deployed-path diagnosis: is the proxy chain adding a tail the
//! `rtp_mux`-direct arms cannot see, at matched end-to-end client-to-echo RTT?
//!
//! This scenario is an **instrument**, not a mandate. `proxy` owns no
//! performance bound: the tri-mandate constitution's bounds, arms and
//! derivations live in `rtp_mux/GATE.md` §Performance and are never restated
//! here. What this scenario asserts is only its own instrument sanity
//! (something was measured, nothing was left unanswered, every echo matched,
//! every arm dialed, the topologies' base RTTs really are matched — including
//! the protocol-only arm's against its regime's calibration — and the emulated
//! capacity is really achievable on the direct arm) and its own echo/delivery
//! integrity.
//! The mandate metrics are **reported**, and any tail this scenario finds is a
//! finding *for `rtp_mux`*, reported as a target with evidence.
//!
//! # What it drives
//!
//! Three topologies, the same impaired hop, the same load shape:
//!
//! - **proxy chain** — the real `proxy` binary, run from a real config file
//!   (`access_server.tcp_server` -> `stream.upstream` hop `rtpmux://…` ->
//!   `proxy_server.rtp_mux_server` -> a loopback TCP echo upstream). Traffic
//!   enters through the access server's own TCP listener, exactly as the
//!   operator's client does, and every byte crosses the chain's `rtp_mux` hop.
//! - **direct transport** — an `rtp_mux` connection with no proxy in the path,
//!   built from the same public `rtp_mux` server/connector the chain's hop uses,
//!   with the same echo shape.
//! - **direct protocol** — the same binary, run from a config that declares
//!   only `proxy_server` listeners, entered by the harness writing the proxy
//!   protocol itself (flow-kind byte, upgrade preamble, relay header). It is
//!   the chain minus its access-server ingress, so a charge it carries is not
//!   the ingress stage's.
//!
//! All topologies' `rtp_mux` hop is impaired by the same seeded
//! `netem_test` instrument, applied to the hop's *two* lanes (interactive and
//! its adjacent bulk port) through one `NetemPair` each.
//!
//! # Controls
//!
//! A chain-versus-direct delta is only attributable with controls that add one
//! stage at a time to the direct arm: `direct_tcp_front` (the client-side
//! kernel-TCP front the access server interposes), `direct_relay` (the
//! server-side byte relay the proxy server interposes, without the protocol)
//! and `direct_front_relay` (both stages stacked — the chain minus the proxy
//! protocol, the access-server chain plumbing and the proxy's stream wrappers).
//! The last one varies two dimensions from the baseline at once and is labelled
//! a composite. Its two relay-stack siblings — `direct_front_relay_tout` and
//! `direct_front_relay_timed` — keep that topology and vary exactly one
//! dimension from it: the relay implementation, from the harness's plain
//! `tokio::io::copy_bidirectional` to the proxy's production stack one wrapper
//! layer at a time.
//!
//! `cadence_steady` opens the measured window only after one whole
//! request/response round trip has completed, so both topologies are measured
//! on an established path. It is the reading that separates a slow path from a
//! path whose connection setup is charged to the first messages of the window —
//! the direct arms establish their mux stream inside `connect()`, the chain
//! arm's `connect()` is a TCP accept at the access server.
//!
//! # The cold-connection reading
//!
//! That asymmetry is what the establishment charge is measured with. Each arm
//! records what its `connect()` did — the `rtp_mux` lane pairing and the proxy
//! protocol where the topology performs them itself, and whether the dial
//! found a live session instead of pairing one — and `connect_ms + first echo`
//! is then the same clock on every topology: the client's first act of
//! connecting to its first echo. `direct_proto` and `direct_transport` show
//! the parts on this side of the process boundary; the chain's parts run inside
//! the binary and appear in its first sample instead. A dedicated table prints
//! connect / lane pairing / protocol / first echo / total per regime, and every
//! arm asserts it dialed at least once, so an arm whose establishment reading
//! was lost cannot pass silently.
//!
//! # Matched RTT
//!
//! The chain's extra hops are loopback TCP, but "loopback is negligible" is an
//! assumption, so the scenario measures it instead: it runs the direct arm,
//! compares the achieved base (min) client-to-echo RTT against the chain's, and
//! re-runs the direct arm with the direct link's one-way delay corrected by
//! half the difference. Both achieved base RTTs and the applied correction are
//! reported, and the arms are asserted to be matched within `RTT_MATCH_TOL`.
//! Only then is the residual difference attributable to the proxy layer.
//!
//! # Shape
//!
//! Two shapes probe the differences `server/config.toml` names and the direct
//! arms never exercise: a request/response shape at depth 1 (the field
//! client's shape) and a pipelined ~5 ms cadence; plus a many-flows arm (four
//! concurrent access flows multiplexed onto the hop) and a rate-shaped bulk
//! arm. The bulk arm states its emulated capacity explicitly and asserts the
//! direct arm reaches it, so a chain fraction against a rate nobody achieved
//! cannot be reported vacuously.
//!
//! # Running it
//!
//! ```sh
//! cargo test --release -p server --test proxy_path_perf -- --ignored --nocapture
//! ```
//!
//! Evidence: a human table on stdout and `report.json` under
//! `$CARGO_TARGET_DIR/proxy_path_perf/` (or `PROXY_PATH_PERF_OUT`). Tier, cost
//! and coverage are declared in `GATE.md`.
//!
//! Fault injection for the vacuity demonstration:
//! `PROXY_PATH_PERF_FAULT=zero_samples` empties one arm's samples,
//! `PROXY_PATH_PERF_FAULT=unanswered` drops responses,
//! `PROXY_PATH_PERF_FAULT=warm_unanswered` leaves the steady arm's warm-up round
//! trip unanswered, and `PROXY_PATH_PERF_FAULT=undialed` discards the arm's dial
//! record after it ran; each must fail the shared instrument-sanity guard.

use std::{
    future::Future,
    io,
    net::{Ipv4Addr, SocketAddr},
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

use common::{
    header::{codec::timed_write_header_async, preamble},
    proxy_runtime::{addr::RouteAddr, header::StreamRequestHeader},
};
use netem_test::{NetemConfig, NetemPair};
use protocol::stream_proto::streams::mux::{MuxFlowKind, write_flow_kind};
use rtp_mux::{
    LaneClass, ObfuscationKey, RtpMuxConnector, RtpMuxConnectorConfig, RtpMuxServer,
    RtpMuxServerConfig, SessionSpawner,
};
use serde_json::json;
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    task::JoinSet,
};

/// The proxy chain's `rtp_mux` hop obfuscation / header key. Any fixed key
/// works; both ends of the chain take it from the config.
const HEADER_KEY: &str = "cHJveHktZXhhbXBsZS1rZXk";
/// The hop's datagram-obfuscation key, used by both topologies so the wire
/// shapes are comparable.
const OBFUSCATION_KEY: [u8; 32] = [0x42; 32];

/// Interactive message size, in bytes. Matches the tri-mandate arms' 256 B
/// interactive payload so the reported percentiles are comparable to theirs.
const MESSAGE_BYTES: usize = 256;

/// Instrument-sanity tolerance for the matched-RTT check: the two topologies'
/// achieved base RTTs must agree within this, or the delta below is measuring
/// path length rather than the proxy layer.
const RTT_MATCH_TOL: Duration = Duration::from_millis(20);

/// Bulk arm: the emulated capacity the rate shaper is configured with, and the
/// steady-state measurement window. The capacity is a *stated* number, and the
/// direct arm must reach it or the chain's fraction is meaningless.
const BULK_CAPACITY_BPS: u64 = 8_000_000;
const BULK_WARMUP: Duration = Duration::from_secs(3);
const BULK_WINDOW: Duration = Duration::from_secs(5);
/// The direct transport must reach this fraction of the stated capacity for the
/// rate shaper to be a usable denominator. This is an assertion about the
/// *instrument* (the emulated capacity is achievable), not about the product.
const DIRECT_BULK_SATURATION: f64 = 0.5;

// ───────────────────────────── port allocation ────────────────────────────

/// Allocate a pair of adjacent free UDP ports, released before use. `rtp_mux`
/// addresses its bulk lane as the interactive port plus one, so the pair is
/// the unit that has to be free.
fn alloc_adjacent_udp_pair() -> (u16, u16) {
    for _ in 0..256 {
        let Ok(first) = std::net::UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)) else {
            continue;
        };
        let port = first.local_addr().unwrap().port();
        match port.checked_add(1) {
            None => continue,
            Some(next) => {
                let Ok(second) = std::net::UdpSocket::bind((Ipv4Addr::LOCALHOST, next)) else {
                    continue;
                };
                drop(first);
                drop(second);
                return (port, next);
            }
        }
    }
    panic!("no adjacent free UDP port pair in 256 draws");
}

fn alloc_tcp_port() -> u16 {
    let listener = std::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener);
    port
}

fn localhost(port: u16) -> SocketAddr {
    SocketAddr::new(Ipv4Addr::LOCALHOST.into(), port)
}

// ──────────────────────────────── regimes ─────────────────────────────────

/// An impairment + scale regime. One dimension per regime, varied from the
/// stated baseline, so a result attributes to that dimension.
#[derive(Clone, Copy)]
struct Regime {
    name: &'static str,
    /// One-way delay added to every datagram on the impaired hop.
    owd: Duration,
    jitter: Duration,
    /// Independent per-datagram loss threshold (`u32::MAX` = 100 %).
    loss: u32,
    /// Rate limit in bits/s for the impaired hop; `0` disables shaping.
    rate_bps: u64,
    seed: u64,
}

impl Regime {
    /// The tri-mandate `clean` arm's impairment: 2 % iid loss, 25 ms one-way,
    /// 5 ms jitter, unshaped.
    const fn clean25() -> Self {
        Self {
            name: "clean25",
            owd: Duration::from_millis(25),
            jitter: Duration::from_millis(5),
            loss: u32::MAX / 50,
            rate_bps: 0,
            seed: 42,
        }
    }

    /// The field's scale: ~190 ms client-to-echo ⇒ ~100 ms one-way.
    const fn field100() -> Self {
        Self {
            name: "field100",
            owd: Duration::from_millis(100),
            jitter: Duration::from_millis(5),
            loss: u32::MAX / 50,
            rate_bps: 0,
            seed: 43,
        }
    }

    /// [`Self::clean25`] with **no loss** — one dimension varied, so a tail
    /// that appears here cannot be a loss-realization artifact of the seeded
    /// drop pattern. It names itself: reporting the lossy and the lossless arm
    /// under one label (`clean25`) made the per-arm table's two rows
    /// indistinguishable, while `GATE.md`'s coverage table already called this
    /// arm `jitter25`.
    const fn jitter25() -> Self {
        Self {
            name: "jitter25",
            loss: 0,
            ..Self::clean25()
        }
    }

    /// [`Self::clean25`] plus the stated bulk capacity.
    const fn clean25_shaped() -> Self {
        Self {
            name: "clean25_shaped",
            rate_bps: BULK_CAPACITY_BPS,
            ..Self::clean25()
        }
    }

    fn c2s(&self, extra_owd: Duration) -> NetemConfig {
        NetemConfig {
            latency: self.owd + extra_owd,
            jitter: self.jitter,
            loss: self.loss,
            rate: self.rate_bps,
            seed: self.seed,
            ..NetemConfig::default()
        }
    }

    fn s2c(&self, extra_owd: Duration) -> NetemConfig {
        NetemConfig {
            seed: self.seed.wrapping_add(1),
            ..self.c2s(extra_owd)
        }
    }
}

// ────────────────────────── the impaired rtp_mux hop ──────────────────────

/// One impaired `rtp_mux` hop: a `NetemPair` on each of the hop's two lanes.
struct ImpairedHop {
    interactive: NetemPair,
    bulk: NetemPair,
    /// The address a client dials to reach the hop (the interactive lane's
    /// netem client-side socket; the bulk lane is derived as port + 1).
    client_addr: SocketAddr,
}

impl ImpairedHop {
    fn spawn(server_interactive: SocketAddr, c2s: &NetemConfig, s2c: &NetemConfig) -> Self {
        let (client_port, bulk_client_port) = alloc_adjacent_udp_pair();
        let bulk_server = localhost(server_interactive.port() + 1);
        let interactive = NetemPair::spawn_on(
            server_interactive,
            c2s.clone(),
            s2c.clone(),
            localhost(client_port),
            localhost(0),
        )
        .expect("bind the interactive-lane netem pair");
        let bulk = NetemPair::spawn_on(
            bulk_server,
            c2s.clone(),
            s2c.clone(),
            localhost(bulk_client_port),
            localhost(0),
        )
        .expect("bind the bulk-lane netem pair");
        let client_addr = interactive.client_addr();
        Self {
            interactive,
            bulk,
            client_addr,
        }
    }

    fn stop(self) {
        self.interactive.stop();
        self.bulk.stop();
    }
}

// ───────────────────────────── echo upstream ──────────────────────────────

/// An explicitly owned, actively reaped task scope. Every background task a test
/// object starts lives in one of these, so nothing is detached into the runtime
/// and a panicked task surfaces on the next [`TaskScope::reap_ready`] rather
/// than being swallowed. Locking is only for the spawn and the non-blocking
/// reap, so the scope never parks a caller.
#[derive(Clone)]
struct TaskScope {
    tasks: Arc<std::sync::Mutex<JoinSet<()>>>,
}

impl TaskScope {
    fn new() -> Self {
        Self {
            tasks: Arc::new(std::sync::Mutex::new(JoinSet::new())),
        }
    }

    fn spawn<F>(&self, fut: F)
    where
        F: Future<Output = ()> + Send + 'static,
    {
        self.tasks.lock().unwrap().spawn(fut);
    }

    /// The `rtp_mux` session spawner backed by this scope.
    fn session_spawner(&self) -> SessionSpawner {
        let scope = self.clone();
        SessionSpawner::new(move |fut| scope.spawn(fut))
    }

    /// Reap every task that has finished, re-raising a panic. Non-blocking, so
    /// a long-lived listener task does not park the caller.
    fn reap_ready(&self) {
        let mut tasks = self.tasks.lock().unwrap();
        while let Some(joined) = tasks.try_join_next() {
            joined.expect("a scoped task panicked");
        }
    }
}

/// A loopback TCP echo server: reads and writes back, counts bytes. This is the
/// chain's upstream destination; the direct arm uses the in-process equivalent.
async fn spawn_tcp_echo() -> (SocketAddr, TaskScope) {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
    let addr = listener.local_addr().unwrap();
    let scope = TaskScope::new();
    let accepted = scope.clone();
    scope.spawn(async move {
        loop {
            let Ok((mut stream, _)) = listener.accept().await else {
                return;
            };
            accepted.spawn(async move {
                let mut buf = vec![0u8; 64 * 1024];
                loop {
                    match stream.read(&mut buf).await {
                        Ok(0) | Err(_) => return,
                        Ok(n) => {
                            if stream.write_all(&buf[..n]).await.is_err() {
                                return;
                            }
                        }
                    }
                }
            });
        }
    });
    (addr, scope)
}

// ─────────────────────────── the proxy chain arm ──────────────────────────

struct ChainHandle {
    child: tokio::process::Child,
    hop: ImpairedHop,
    access_addr: SocketAddr,
    dir: PathBuf,
}

fn write_chain_config(
    dir: &Path,
    hop_addr: SocketAddr,
    proxy_addr: SocketAddr,
    access_port: u16,
    echo: SocketAddr,
) -> PathBuf {
    std::fs::create_dir_all(dir).unwrap();
    let config = format!(
        r#"[stream.upstream]
"hop" = {{ address = "rtpmux://{hop_addr}", header_key = "{HEADER_KEY}" }}

[access_server.stream.conn_selector]
"default" = {{ chains = [ {{ weight = 1, chain = ["hop"] }} ], probe_rtt = false, active_chains = 1 }}

[[access_server.tcp_server]]
listen_addr = "127.0.0.1:{access_port}"
destination = "tcp://{echo}"
conn_selector = "default"

[[proxy_server.rtp_mux_server]]
listen_addr = "127.0.0.1:{proxy_port}"
header_key = "{HEADER_KEY}"
allow_loopback = true
"#,
        proxy_port = proxy_addr.port(),
    );
    let path = dir.join("config.toml");
    std::fs::write(&path, config).unwrap();
    path
}

/// Start the real `proxy` binary from a real config file, with the chain's
/// `rtp_mux` hop impaired by `c2s`/`s2c`, and return a handle whose drop kills
/// the process and the netem threads.
async fn start_chain(
    tag: &str,
    regime: &Regime,
    extra_owd: Duration,
    echo: SocketAddr,
) -> ChainHandle {
    let (proxy_port, _proxy_bulk) = alloc_adjacent_udp_pair();
    let proxy_addr = localhost(proxy_port);
    let hop = ImpairedHop::spawn(proxy_addr, &regime.c2s(extra_owd), &regime.s2c(extra_owd));
    let access_port = alloc_tcp_port();
    let dir = unique_temp_dir(tag);
    let path = write_chain_config(&dir, hop.client_addr, proxy_addr, access_port, echo);

    let child = tokio::process::Command::new(env!("CARGO_BIN_EXE_proxy"))
        .arg(&path)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .expect("spawn the proxy binary");

    // Wait for the access server's own listener to accept.
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        if TcpStream::connect(localhost(access_port)).await.is_ok() {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the proxy binary never opened its access listener"
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    ChainHandle {
        child,
        hop,
        access_addr: localhost(access_port),
        dir,
    }
}

impl ChainHandle {
    async fn shutdown(mut self) {
        let _ = self.child.kill().await;
        self.hop.stop();
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn unique_temp_dir(tag: &str) -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    std::env::temp_dir().join(format!(
        "proxy-path-perf-{tag}-{}-{nanos}",
        std::process::id()
    ))
}

// ───────────────────────── the direct transport arm ───────────────────────

/// How one relay leg moves bytes between its two sockets.
///
/// `Tokio` is the harness's own plain `tokio::io::copy_bidirectional`.
/// `ProxyTimeout` and `ProxyTimed` add the production relay stack the proxy's
/// two services run, one wrapper layer at a time: `ProxyTimeout` is the
/// proxy's `TimeoutStreamShared` around each end plus the proxy's own
/// `copy_bidirectional` fork, and `ProxyTimed` is that plus the
/// `async_speed_limit::Limiter` the services wrap their relay in —
/// `Limiter::new(f64::INFINITY)`, the unlimited value both services configure
/// when no `speed_limit` is set. So each relay-stack arm varies exactly one
/// dimension from the plain composite control: the relay implementation, with
/// topology, impairment, cadence and matched RTT held.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum RelayImpl {
    Tokio,
    ProxyTimeout,
    ProxyTimed,
}

/// The direct arm's server-side handler mode for an accepted mux stream.
const IN_PROCESS_ECHO: u8 = 0;
const RELAY_TOKIO: u8 = 1;
const RELAY_PROXY_TIMEOUT: u8 = 2;
const RELAY_PROXY_TIMED: u8 = 3;

/// The relay implementation a non-echo handler mode selects.
fn relay_impl_of(mode: u8) -> RelayImpl {
    match mode {
        RELAY_PROXY_TIMEOUT => RelayImpl::ProxyTimeout,
        RELAY_PROXY_TIMED => RelayImpl::ProxyTimed,
        _ => RelayImpl::Tokio,
    }
}

/// Move bytes both ways with `impl_`'s relay implementation.
async fn relay_leg<A, B>(a: A, b: B, impl_: RelayImpl)
where
    A: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    B: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    use common::proxy_runtime::relay::copy as proxy_copy;
    match impl_ {
        RelayImpl::Tokio => {
            let (mut a, mut b) = (a, b);
            let _ = tokio::io::copy_bidirectional(&mut a, &mut b).await;
        }
        RelayImpl::ProxyTimeout => {
            let mut a = proxy_copy::TimeoutStreamShared::new(a);
            let mut b = proxy_copy::TimeoutStreamShared::new(b);
            a.set_timeout(Some(common::STREAM_IO_TIMEOUT));
            b.set_timeout(Some(common::STREAM_IO_TIMEOUT));
            let mut a = Box::pin(a);
            let mut b = Box::pin(b);
            let _ = proxy_copy::copy_bidirectional(&mut a, &mut b).await;
        }
        RelayImpl::ProxyTimed => {
            let _ = proxy_copy::timed_copy_bidirectional(
                a,
                b,
                async_speed_limit::Limiter::new(f64::INFINITY),
            )
            .await;
        }
    }
}

struct DirectHandle {
    scope: TaskScope,
    hop: ImpairedHop,
    connector: Arc<RtpMuxConnector>,
    addr: SocketAddr,
    /// Which handler the accepted mux stream gets: the in-process echo, or a
    /// byte relay to the TCP echo server under one of the [`RelayImpl`]
    /// implementations. Flipped between arms so one server can serve all of
    /// them.
    relay_mode: Arc<std::sync::atomic::AtomicU8>,
}

/// Start a bare `rtp_mux` server whose accepted streams are echoed (or, with
/// `relay_mode` set, relayed to the TCP echo server), impaired by the same
/// `NetemPair` shape as the chain's hop.
async fn start_direct(regime: &Regime, extra_owd: Duration, echo: SocketAddr) -> DirectHandle {
    let scope = TaskScope::new();
    let spawner = scope.session_spawner();
    let server = RtpMuxServer::bind(
        "127.0.0.1:0",
        RtpMuxServerConfig {
            obfuscation_key: Some(ObfuscationKey::from_bytes(OBFUSCATION_KEY)),
        },
    )
    .await
    .expect("bind the direct rtp_mux server");
    let server_addr = server.listener().local_addr();
    let relay_mode = Arc::new(std::sync::atomic::AtomicU8::new(IN_PROCESS_ECHO));
    let server_relay_mode = Arc::clone(&relay_mode);
    let handler_scope = scope.clone();
    scope.spawn(async move {
        let _ = server
            .serve(spawner, move |stream| {
                let mode = server_relay_mode.load(Ordering::Relaxed);
                handler_scope.spawn(async move {
                    if mode != IN_PROCESS_ECHO {
                        let Ok(tcp) = TcpStream::connect(echo).await else {
                            return;
                        };
                        relay_leg(stream, tcp, relay_impl_of(mode)).await;
                        return;
                    }
                    let mut stream = stream;
                    let mut buf = vec![0u8; 64 * 1024];
                    loop {
                        match stream.read(&mut buf).await {
                            Ok(0) | Err(_) => return,
                            Ok(n) => {
                                if stream.write_all(&buf[..n]).await.is_err() {
                                    return;
                                }
                            }
                        }
                    }
                });
            })
            .await;
    });

    let hop = ImpairedHop::spawn(server_addr, &regime.c2s(extra_owd), &regime.s2c(extra_owd));

    let bind: rtp_mux::BindSelector = Arc::new(|_addr: SocketAddr| localhost(0));
    let (connector, driver) = RtpMuxConnector::with_config(
        RtpMuxConnectorConfig::standard(bind)
            .with_obfuscation_key(Some(ObfuscationKey::from_bytes(OBFUSCATION_KEY))),
    );
    let connector = Arc::new(connector);
    scope.spawn(driver);
    DirectHandle {
        scope,
        addr: hop.client_addr,
        hop,
        connector,
        relay_mode,
    }
}

impl DirectHandle {
    fn shutdown(self) {
        self.hop.stop();
    }
}

// ──────────────────── the proxy-protocol-only arm ─────────────────────────

/// The chain minus its access-server ingress: the real `proxy` binary run
/// from a config file that declares **only** `proxy_server` listeners, with the
/// same impaired `rtp_mux` hop, and the harness itself acting as the chain's
/// first hop — pairing the mux session and writing the flow-kind byte, the
/// upgrade preamble and the relay header that
/// `common::proxy_runtime::client::stream::establish` writes for the deployed
/// chain. Everything after the client's first write is the deployed binary's
/// own `proxy_server` path: the protocol read, the upstream connect and the
/// production relay.
///
/// It answers the one question the chain-versus-direct pair cannot: is the
/// chain's establishment charge its *ingress stage* (the TCP accept at the
/// access server, the chain selection, the pool) or the steps every topology
/// pays (the `rtp_mux` lane pairing, the preamble and header, the upstream
/// connect)?
///
/// Process readiness is observed through a second, TCP `proxy_server` listener
/// in the same config. It is built in the same prepare pass as the `rtp_mux`
/// listener and spawned in the same commit, so accepting on it proves the mux
/// listener is bound; a bare TCP connect that is closed before any preamble is
/// written puts no traffic on the impaired hop and no session on the mux
/// listener. Observing readiness through the *access* server's listener
/// instead would make the probe itself drive a chain establishment on the very
/// hop the arm measures.
struct ProtoServerHandle {
    child: tokio::process::Child,
    hop: ImpairedHop,
    /// The address a client dials to reach the proxy server through the
    /// impaired hop (the hop's client-side socket).
    addr: SocketAddr,
    connector: Arc<RtpMuxConnector>,
    scope: TaskScope,
    dir: PathBuf,
}

/// The config for the `direct_proto` arm: `proxy_server` listeners only, and
/// no access server at all.
fn write_proto_config(dir: &Path, proxy_addr: SocketAddr, probe_port: u16) -> PathBuf {
    std::fs::create_dir_all(dir).unwrap();
    let config = format!(
        r#"[[proxy_server.rtp_mux_server]]
listen_addr = "127.0.0.1:{proxy_port}"
header_key = "{HEADER_KEY}"
allow_loopback = true

# Process-readiness probe only; see `ProtoServerHandle`.
[[proxy_server.tcp_server]]
listen_addr = "127.0.0.1:{probe_port}"
header_key = "{HEADER_KEY}"
allow_loopback = true
"#,
        proxy_port = proxy_addr.port(),
    );
    let path = dir.join("config.toml");
    std::fs::write(&path, config).unwrap();
    path
}

/// Start the real `proxy` binary with `proxy_server` listeners only, with the
/// mux listener reached through `c2s`/`s2c`, and a fresh mux connector whose
/// obfuscation key is the listener's own header key.
async fn start_proto_server(
    tag: &str,
    regime: &Regime,
    extra_owd: Duration,
    proto: &ProtoClient,
) -> ProtoServerHandle {
    let (proxy_port, _proxy_bulk) = alloc_adjacent_udp_pair();
    let proxy_addr = localhost(proxy_port);
    let hop = ImpairedHop::spawn(proxy_addr, &regime.c2s(extra_owd), &regime.s2c(extra_owd));
    let probe_port = alloc_tcp_port();
    let dir = unique_temp_dir(tag);
    let path = write_proto_config(&dir, proxy_addr, probe_port);

    let child = tokio::process::Command::new(env!("CARGO_BIN_EXE_proxy"))
        .arg(&path)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .expect("spawn the proxy binary");

    // Wait for the same process's TCP proxy-server listener to accept: it is
    // prepared and committed with the mux listener, so this is the process's
    // own readiness, not a guess at a sleep.
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        if TcpStream::connect(localhost(probe_port)).await.is_ok() {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the proxy binary never opened its readiness listener"
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }

    let scope = TaskScope::new();
    let bind: rtp_mux::BindSelector = Arc::new(|_addr: SocketAddr| localhost(0));
    let (connector, driver) = RtpMuxConnector::with_config(
        RtpMuxConnectorConfig::standard(bind)
            .with_obfuscation_key(Some(ObfuscationKey::from_bytes(*proto.header_crypto.key()))),
    );
    let connector = Arc::new(connector);
    scope.spawn(driver);
    ProtoServerHandle {
        child,
        addr: hop.client_addr,
        hop,
        connector,
        scope,
        dir,
    }
}

impl ProtoServerHandle {
    async fn shutdown(mut self) {
        let _ = self.child.kill().await;
        self.hop.stop();
        self.scope.reap_ready();
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

// ───────────────────────────── load shapes ────────────────────────────────

#[derive(Clone, Copy, Debug)]
enum Shape {
    /// One outstanding request, as the field client behaves.
    RoundTrip { window: Duration },
    /// ~5 ms pipelined cadence, the tri-mandate interactive cadence.
    Cadence {
        window: Duration,
        interval: Duration,
    },
    /// The same pipelined cadence, but with one complete request/response
    /// round trip taken **before** the measured window opens.
    ///
    /// This is the steady-state reading of the plain cadence, and the one
    /// dimension that separates "the path is slow once it is up" from "the
    /// path's connection establishment is charged to the first messages of the
    /// window". The direct arms establish their mux stream inside
    /// `Target::connect()`, i.e. before their window opens; the chain arm's
    /// `connect()` is a TCP accept at the access server, so its window opens
    /// while the chain is still being built. Warming both topologies removes
    /// that asymmetry.
    CadenceSteady {
        window: Duration,
        interval: Duration,
    },
    /// `flows` concurrent round-trip flows — the multiplexed access-flow shape.
    Flows { flows: usize, window: Duration },
    /// A saturating bulk upload with the echo drained concurrently, measured
    /// over a steady-state window rather than end-to-end: the goodput is the
    /// echo counter's delta across the window, sampled while the pump still
    /// runs, so the connection ramp is not divided into the reading.
    Bulk { warmup: Duration, window: Duration },
}

impl Shape {
    fn name(self) -> String {
        match self {
            Shape::RoundTrip { .. } => "rr".into(),
            Shape::Cadence { .. } => "cadence".into(),
            Shape::CadenceSteady { .. } => "cadence_steady".into(),
            Shape::Flows { flows, .. } => format!("flows{flows}"),
            Shape::Bulk { .. } => "bulk".into(),
        }
    }
    fn is_bulk(self) -> bool {
        matches!(self, Shape::Bulk { .. })
    }
}

trait DynStream: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send> DynStream for T {}

fn elapsed_ms(since: Instant) -> f64 {
    since.elapsed().as_secs_f64() * 1000.0
}

/// The proxy protocol's client half, as the deployed chain's first hop speaks
/// it: a flow-kind byte, the upgrade preamble, then the relay header naming the
/// destination. `common::proxy_runtime::client::stream::establish` writes
/// exactly these three things on a chain's first hop, so a harness that writes
/// them itself puts the same bytes on the wire as the access server would.
struct ProtoClient {
    /// The key the `proxy_server` listener is configured with; it both signs
    /// the relay header and obfuscates the `rtp_mux` transport to that listener.
    header_crypto: tokio_chacha20::config::Config,
    /// The destination the relay header asks for: the same `tcp://<echo>` the
    /// chain's `access_server.tcp_server` is configured with.
    upstream: RouteAddr,
}

/// The protocol client the `direct_proto` arm speaks with.
fn proto_client(echo: SocketAddr) -> ProtoClient {
    ProtoClient {
        header_crypto: tokio_chacha20::config::ConfigBuilder(HEADER_KEY.to_string())
            .build()
            .expect("the config's own header key must build"),
        upstream: format!("tcp://{echo}")
            .parse()
            .expect("the echo destination parses as a route address"),
    }
}

/// Write the three wire elements a chain's first hop writes to its next hop.
async fn speak_proxy_protocol<S>(
    stream: &mut S,
    header_crypto: &tokio_chacha20::config::Config,
    upstream: &RouteAddr,
) -> io::Result<()>
where
    S: AsyncWrite + Unpin,
{
    write_flow_kind(stream, MuxFlowKind::Stream).await?;
    preamble::send_upgrade(stream, common::STREAM_IO_TIMEOUT, header_crypto)
        .await
        .map_err(io::Error::other)?;
    let header = StreamRequestHeader {
        upstream: Some(upstream.clone()),
    };
    timed_write_header_async(
        stream,
        &header,
        *header_crypto.key(),
        common::STREAM_IO_TIMEOUT,
    )
    .await
    .map_err(io::Error::other)
}

/// What one [`Target::connect`] actually did, and how long each part took.
///
/// The deployed chain's client-side `connect()` is only a TCP accept at the
/// access server: every establishment step after it — the `rtp_mux` lane
/// pairing, the protocol preamble and header, the upstream connect — happens
/// inside the binary, so the arm's measured window has already opened by the
/// time those steps run and they are charged to the arm's first message. The
/// direct arms do their lane pairing inside `connect()`, before the window. One
/// arm's first-message latency therefore cannot be compared to the other's on
/// its own; recording the parts of `connect()` makes the comparison explicit.
/// `connect_ms + the first sample` is the *same* clock on every topology: from
/// the client's first act of connecting to its first echo.
#[derive(Clone, Copy, Debug)]
struct Dial {
    /// Wall time inside `connect()`.
    connect_ms: f64,
    /// The part of it spent in the `rtp_mux` lane pairing
    /// (`connect_stream_with_lane[_and_key]`). `None` when this topology does
    /// not pair a mux session itself.
    mux_dial_ms: Option<f64>,
    /// The part spent speaking the proxy protocol (flow kind, preamble, relay
    /// header). `None` when this topology does not speak it itself.
    protocol_ms: Option<f64>,
    /// Whether a mux session for the peer was already live when the dial
    /// started, i.e. the connector reused a session instead of pairing one. A
    /// reused dial is not a cold-connection measurement.
    mux_session_reused: bool,
}

/// A dialable client: the chain's access listener, the direct transport, or the
/// proxy protocol spoken by the harness itself against a bare `proxy_server`.
#[derive(Clone)]
struct Target {
    kind: TargetKind,
    /// One [`Dial`] per `connect()` this target has served, in call order.
    /// [`run_shape`] takes them when an arm starts, so each arm reports its own
    /// establishment rather than a predecessor's.
    dials: Arc<std::sync::Mutex<Vec<Dial>>>,
}

#[derive(Clone)]
enum TargetKind {
    /// The chain arm: a TCP accept at the access server's listener.
    Access(SocketAddr),
    /// The direct arm: an `rtp_mux` stream on the direct transport.
    Direct {
        connector: Arc<RtpMuxConnector>,
        addr: SocketAddr,
    },
    /// The `direct_proto` arm: an `rtp_mux` stream to the proxy binary's
    /// `proxy_server`, with the harness itself writing the flow-kind byte, the
    /// upgrade preamble and the relay header.
    Proto {
        connector: Arc<RtpMuxConnector>,
        addr: SocketAddr,
        header_crypto: tokio_chacha20::config::Config,
        upstream: RouteAddr,
    },
}

impl Target {
    fn new(kind: TargetKind) -> Self {
        Self {
            kind,
            dials: Arc::new(std::sync::Mutex::new(Vec::new())),
        }
    }

    fn access(addr: SocketAddr) -> Self {
        Self::new(TargetKind::Access(addr))
    }

    fn direct(connector: Arc<RtpMuxConnector>, addr: SocketAddr) -> Self {
        Self::new(TargetKind::Direct { connector, addr })
    }

    fn proto(
        connector: Arc<RtpMuxConnector>,
        addr: SocketAddr,
        header_crypto: tokio_chacha20::config::Config,
        upstream: RouteAddr,
    ) -> Self {
        Self::new(TargetKind::Proto {
            connector,
            addr,
            header_crypto,
            upstream,
        })
    }

    /// Take this target's dial records, leaving it empty.
    fn take_dials(&self) -> Vec<Dial> {
        std::mem::take(&mut *self.dials.lock().unwrap())
    }

    async fn connect(&self) -> Box<dyn DynStream> {
        let started = Instant::now();
        let (stream, mut dial): (Box<dyn DynStream>, Dial) = match &self.kind {
            TargetKind::Access(addr) => (
                Box::new(
                    TcpStream::connect(addr)
                        .await
                        .expect("connect the access listener"),
                ),
                Dial {
                    connect_ms: 0.0,
                    mux_dial_ms: None,
                    protocol_ms: None,
                    mux_session_reused: false,
                },
            ),
            TargetKind::Direct { connector, addr } => {
                let reused = connector.probe_session(*addr).is_some();
                let paired = Instant::now();
                let stream = connector
                    .connect_stream_with_lane(*addr, LaneClass::Interactive)
                    .await
                    .expect("connect the direct rtp_mux stream");
                (
                    Box::new(stream),
                    Dial {
                        connect_ms: 0.0,
                        mux_dial_ms: Some(elapsed_ms(paired)),
                        protocol_ms: None,
                        mux_session_reused: reused,
                    },
                )
            }
            TargetKind::Proto {
                connector,
                addr,
                header_crypto,
                upstream,
            } => {
                let reused = connector.probe_session(*addr).is_some();
                let paired = Instant::now();
                // The obfuscation key is the header key: the `proxy_server`
                // derives its `rtp_mux` listener's key from the same header
                // key, exactly as the chain's first hop passes
                // `header_crypto.key()` to its connector.
                let mut stream = connector
                    .connect_stream_with_lane_and_key(
                        *addr,
                        LaneClass::Interactive,
                        Some(ObfuscationKey::from_bytes(*header_crypto.key())),
                    )
                    .await
                    .expect("connect the direct_proto rtp_mux stream");
                let mux_dial_ms = elapsed_ms(paired);
                let spoke = Instant::now();
                speak_proxy_protocol(&mut stream, header_crypto, upstream)
                    .await
                    .expect("write the proxy protocol to the proxy_server");
                (
                    Box::new(stream),
                    Dial {
                        connect_ms: 0.0,
                        mux_dial_ms: Some(mux_dial_ms),
                        protocol_ms: Some(elapsed_ms(spoke)),
                        mux_session_reused: reused,
                    },
                )
            }
        };
        dial.connect_ms = elapsed_ms(started);
        self.dials.lock().unwrap().push(dial);
        stream
    }
}

fn payload_for(seq: u64) -> [u8; MESSAGE_BYTES] {
    let mut buf = [0u8; MESSAGE_BYTES];
    buf[..8].copy_from_slice(&seq.to_le_bytes());
    for (i, byte) in buf.iter_mut().enumerate().skip(8) {
        *byte = (i as u8) ^ (seq as u8);
    }
    buf
}

/// One arm's measured outcome. Every arm goes through
/// [`ArmOutcome::assert_sane`], including the fault-injection arms, so the
/// vacuity demonstration exercises the same guard the real runs do.
#[derive(Default)]
struct ArmOutcome {
    topology: &'static str,
    regime: &'static str,
    shape: String,
    owd_ms: u64,
    /// Whether this arm measured a bulk transfer rather than per-message
    /// latencies; the two need different sanity checks.
    bulk: bool,
    /// Achieved base (minimum) client-to-echo RTT.
    base_rtt_ms: f64,
    latencies_ms: Vec<f64>,
    unanswered: u64,
    mismatches: u64,
    offered_bytes: u64,
    delivered_bytes: u64,
    wire_interactive_c2s_bytes: u64,
    wire_interactive_s2c_bytes: u64,
    wire_bulk_c2s_bytes: u64,
    netem_dropped: u64,
    netem_delayed: u64,
    netem_forwarded: u64,
    goodput_mib_s: Option<f64>,
    echo_elapsed_s: Option<f64>,
    /// How many `connect()` calls this arm made. Zero means the arm never
    /// dialed, which is how an instrument loses its cold-connection reading
    /// without failing anything else.
    dials: usize,
    /// Wall time inside the arm's first `connect()`, and the two parts of it
    /// this topology performs itself: the `rtp_mux` lane pairing and the proxy
    /// protocol. `None` where the topology does not perform that part.
    connect_ms: Option<f64>,
    mux_dial_ms: Option<f64>,
    protocol_ms: Option<f64>,
    /// Whether the arm's first connect reused a live mux session, i.e. was not
    /// a cold connection.
    mux_session_reused: bool,
    /// The arm's first measured message, and `connect_ms + first` — the same
    /// clock on every topology: from the client's first act of connecting to
    /// its first echo.
    first_ms: Option<f64>,
    cold_total_ms: Option<f64>,
}

impl ArmOutcome {
    fn label(&self) -> String {
        format!("{}/{}", self.topology, self.shape)
    }

    fn percentile(&self, q: f64) -> f64 {
        percentile(&self.latencies_ms, q)
    }

    fn over_ceiling(&self) -> usize {
        self.latencies_ms.iter().filter(|&&v| v > SLOW_MS).count()
    }

    fn wire_multiple(&self) -> f64 {
        if self.offered_bytes == 0 {
            f64::NAN
        } else {
            self.wire_interactive_c2s_bytes as f64 / self.offered_bytes as f64
        }
    }

    /// Record the arm's establishment from its dial log: the first `connect()`,
    /// its two parts this topology performs itself, and the cold-connection
    /// total (`connect` + first echo). The first connect is the cold one on
    /// every arm whose target is fresh, which is every arm that starts its own
    /// process or its own connector; `dials` counts them so a `flows4` arm is
    /// visible as four connections rather than one.
    fn record_dials(&mut self, dials: &[Dial]) {
        self.dials += dials.len();
        let Some(dial) = dials.first() else {
            return;
        };
        self.connect_ms = Some(dial.connect_ms);
        self.mux_dial_ms = dial.mux_dial_ms;
        self.protocol_ms = dial.protocol_ms;
        self.mux_session_reused = dial.mux_session_reused;
        self.first_ms = self.latencies_ms.first().copied();
        self.cold_total_ms = self.first_ms.map(|first| dial.connect_ms + first);
    }

    /// The shared instrument-sanity guard. It fails when nothing was measured,
    /// when a request was left unanswered, or when an echo did not match — the
    /// three ways an instrument silently stops measuring. It asserts nothing
    /// about latency: the mandate bounds live in `rtp_mux/GATE.md`.
    fn assert_sane(&self) {
        if self.bulk {
            assert!(
                self.offered_bytes > 0,
                "INSTRUMENT: bulk arm {} offered zero bytes",
                self.label()
            );
        } else {
            assert!(
                !self.latencies_ms.is_empty(),
                "INSTRUMENT: arm {} measured zero samples",
                self.label()
            );
        }
        assert!(
            self.unanswered == 0,
            "INSTRUMENT: arm {} left {} request(s) unanswered",
            self.label(),
            self.unanswered
        );
        assert!(
            self.mismatches == 0,
            "INSTRUMENT: arm {} had {} echo mismatch(es); delivery integrity broken",
            self.label(),
            self.mismatches
        );
        assert!(
            self.delivered_bytes == self.offered_bytes,
            "INSTRUMENT: arm {} delivered {} of {} offered bytes",
            self.label(),
            self.delivered_bytes,
            self.offered_bytes
        );
        assert!(
            self.dials > 0,
            "INSTRUMENT: arm {} never dialed, so it measured no connection",
            self.label()
        );
    }
}

/// The ceiling above which a sample is reported as slow. The same 250 ms the
/// over-250 ms columns use.
const SLOW_MS: f64 = 250.0;

/// How the slow samples sit in emission order.
///
/// A tail that is an arm's **first** N samples is a start-of-window effect: the
/// connection was still being established while those messages were written,
/// so the measured window — not the steady state — carries the setup cost. A
/// tail **scattered** through the arm is a steady-state stall. Percentiles
/// alone cannot tell the two apart, and the two need opposite responses (fix
/// the instrument versus fix the path), so the diagnosis reports this shape.
struct SlowShape {
    count: usize,
    /// Index of the earliest and latest slow sample, in emission order.
    first: Option<usize>,
    last: Option<usize>,
    /// Maximal runs of consecutive slow samples, and the longest such run.
    episodes: usize,
    max_run: usize,
}

fn slow_shape(latencies: &[f64]) -> SlowShape {
    let mut shape = SlowShape {
        count: 0,
        first: None,
        last: None,
        episodes: 0,
        max_run: 0,
    };
    let mut run = 0usize;
    for (index, value) in latencies.iter().enumerate() {
        if *value > SLOW_MS {
            shape.count += 1;
            shape.first.get_or_insert(index);
            shape.last = Some(index);
            run += 1;
            shape.max_run = shape.max_run.max(run);
        } else {
            if run > 0 {
                shape.episodes += 1;
            }
            run = 0;
        }
    }
    if run > 0 {
        shape.episodes += 1;
    }
    shape
}

fn percentile(sorted: &[f64], q: f64) -> f64 {
    if sorted.is_empty() {
        return f64::NAN;
    }
    let mut v = sorted.to_vec();
    v.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let rank = (q * (v.len() as f64 - 1.0)).round() as usize;
    v[rank.min(v.len() - 1)]
}

/// Run the load shape against a freshly dialed target and collect the outcome.
async fn run_shape(
    topology: &'static str,
    regime: &Regime,
    target: &Target,
    shape: Shape,
    hop: &ImpairedHop,
    fault: Option<Fault>,
) -> ArmOutcome {
    let mut outcome = ArmOutcome {
        topology,
        regime: regime.name,
        shape: shape.name(),
        owd_ms: regime.owd.as_millis() as u64,
        bulk: shape.is_bulk(),
        ..ArmOutcome::default()
    };

    if fault == Some(Fault::ZeroSamples) {
        // Instrument sanity is checked on an arm that measured nothing.
        let mut latencies = Vec::new();
        latencies.clear();
        outcome.latencies_ms = latencies;
        return outcome;
    }

    // Instrument sanity is exercised on an arm that measured nothing.
    if fault == Some(Fault::ZeroSamples) {
        return outcome;
    }

    // Snapshot the hop's counters around the arm so each arm reports its own
    // per-segment wire, even when two arms share one impaired hop.
    let hop_before = hop.interactive.snapshot_c2s();
    let hop_before_s2c = hop.interactive.snapshot_s2c();
    let hop_before_bulk = hop.bulk.snapshot_c2s();
    // Clear the dial log before the shape runs, so what this arm reports is
    // the shape's own connects and never a predecessor arm's.
    let _ = target.take_dials();

    let latencies = match shape {
        Shape::RoundTrip { window } => {
            let (latencies, unanswered) = round_trip_arm(target, window, fault).await;
            outcome.unanswered = unanswered;
            latencies
        }
        Shape::Cadence { window, interval } => {
            let (latencies, unanswered) = cadence_arm(target, window, interval, false, None).await;
            outcome.unanswered = unanswered;
            latencies
        }
        Shape::CadenceSteady { window, interval } => {
            let (latencies, unanswered) = cadence_arm(target, window, interval, true, fault).await;
            outcome.unanswered = unanswered;
            latencies
        }
        Shape::Flows { flows, window } => {
            let (latencies, unanswered) = flows_arm(target, flows, window).await;
            outcome.unanswered = unanswered;
            latencies
        }
        Shape::Bulk { warmup, window } => {
            let (goodput_mib_s, sent, delivered) = bulk_arm(target, warmup, window).await;
            outcome.goodput_mib_s = Some(goodput_mib_s);
            outcome.echo_elapsed_s = Some(window.as_secs_f64());
            outcome.offered_bytes = sent;
            outcome.delivered_bytes = delivered;
            Vec::new()
        }
    };

    // Every non-bulk shape reports one latency per completed request and books
    // the request/response payload as offered/delivered bytes.
    if !shape.is_bulk() {
        outcome.offered_bytes = latencies.len() as u64 * MESSAGE_BYTES as u64;
        outcome.delivered_bytes = outcome.offered_bytes;
    }
    outcome.latencies_ms = latencies;

    let c2s = hop.interactive.snapshot_c2s();
    let s2c = hop.interactive.snapshot_s2c();
    let bulk = hop.bulk.snapshot_c2s();
    outcome.wire_interactive_c2s_bytes = c2s
        .stats
        .forwarded_bytes
        .saturating_sub(hop_before.stats.forwarded_bytes);
    outcome.wire_interactive_s2c_bytes = s2c
        .stats
        .forwarded_bytes
        .saturating_sub(hop_before_s2c.stats.forwarded_bytes);
    outcome.wire_bulk_c2s_bytes = bulk
        .stats
        .forwarded_bytes
        .saturating_sub(hop_before_bulk.stats.forwarded_bytes);
    outcome.netem_dropped = c2s.stats.dropped.saturating_sub(hop_before.stats.dropped)
        + s2c
            .stats
            .dropped
            .saturating_sub(hop_before_s2c.stats.dropped);
    outcome.netem_delayed = c2s.stats.delayed.saturating_sub(hop_before.stats.delayed)
        + s2c
            .stats
            .delayed
            .saturating_sub(hop_before_s2c.stats.delayed);
    outcome.netem_forwarded = c2s
        .stats
        .forwarded
        .saturating_sub(hop_before.stats.forwarded)
        + s2c
            .stats
            .forwarded
            .saturating_sub(hop_before_s2c.stats.forwarded);
    outcome.base_rtt_ms = outcome
        .latencies_ms
        .iter()
        .copied()
        .fold(f64::INFINITY, f64::min);
    if !outcome.base_rtt_ms.is_finite() {
        outcome.base_rtt_ms = 0.0;
    }
    let mut dials = target.take_dials();
    if fault == Some(Fault::Undialed) {
        // The instrument's own failure mode for the establishment reading: the
        // arm ran and measured, but its dial record is gone, so it has no
        // cold-connection reading to report.
        dials.clear();
    }
    outcome.record_dials(&dials);
    outcome
}

/// A request/response shape at depth one: write one message, read the echo,
/// repeat until the window closes. Returns one latency per completed exchange
/// and the number of requests that were never answered.
async fn round_trip_arm(
    target: &Target,
    window: Duration,
    fault: Option<Fault>,
) -> (Vec<f64>, u64) {
    let mut stream = target.connect().await;
    let deadline = Instant::now() + window;
    let mut latencies = Vec::new();
    let mut unanswered = 0u64;
    let mut seq = 0u64;
    let mut buf = [0u8; MESSAGE_BYTES];
    while Instant::now() < deadline {
        let sent = payload_for(seq);
        let start = Instant::now();
        if fault == Some(Fault::Unanswered) && seq >= 3 {
            // The instrument's own failure mode: a request is issued and never
            // answered, after a few real exchanges so the guard is exercised on
            // a measured arm rather than on an empty one.
            let _ = stream.write_all(&sent).await;
            unanswered += 1;
            break;
        }
        if stream.write_all(&sent).await.is_err() {
            break;
        }
        match tokio::time::timeout(Duration::from_secs(30), stream.read_exact(&mut buf)).await {
            Ok(Ok(_)) => {}
            Ok(Err(_)) | Err(_) => {
                // The request went out and nothing ever came back.
                unanswered += 1;
                break;
            }
        }
        latencies.push(start.elapsed().as_secs_f64() * 1000.0);
        assert_eq!(
            &buf[..],
            &sent[..],
            "INSTRUMENT: request/response payload mismatch at seq {seq}"
        );
        seq += 1;
    }
    (latencies, unanswered)
}

/// A pipelined cadence: writes are paced at `interval` regardless of replies,
/// so several requests are in flight, and each reply's latency is measured from
/// its own write. Returns one latency per completed exchange and the number of
/// requests whose reply never arrived. With `warm`, one whole request/response
/// round trip is taken on the connection before the window opens, so the reading
/// is the established path rather than the connection setup.
async fn cadence_arm(
    target: &Target,
    window: Duration,
    interval: Duration,
    warm: bool,
    fault: Option<Fault>,
) -> (Vec<f64>, u64) {
    let mut stream = target.connect().await;
    if warm {
        // The echo of `MESSAGE_BYTES` proves the whole path is up. A distinct
        // payload keeps the warm-up from being confused with a measured
        // sample; the measured sequence still starts at 0 below.
        let sent = payload_for(u64::MAX);
        let mut buf = [0u8; MESSAGE_BYTES];
        let answered = if fault == Some(Fault::WarmUnanswered) {
            false
        } else {
            stream.write_all(&sent).await.is_ok()
                && matches!(
                    tokio::time::timeout(Duration::from_secs(30), stream.read_exact(&mut buf))
                        .await,
                    Ok(Ok(_))
                )
        };
        if !answered {
            // An unanswered warm-up is an instrument failure: report it as an
            // unanswered request so the shared guard fails the arm.
            return (Vec::new(), 1);
        }
        assert_eq!(
            &buf[..],
            &sent[..],
            "INSTRUMENT: warm-up payload mismatch before the cadence window"
        );
    }
    let (mut reader, mut writer) = tokio::io::split(stream);
    let (tx, mut rx) = tokio::sync::mpsc::channel::<Instant>(65536);
    let send_deadline = Instant::now() + window;
    let writer_finished = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let writer_finished_flag = Arc::clone(&writer_finished);
    let drain_done = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let writer_drain = Arc::clone(&drain_done);
    let mut tasks = JoinSet::new();
    tasks.spawn(async move {
        let mut seq = 0u64;
        let mut ticker = tokio::time::interval(interval);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        while Instant::now() < send_deadline {
            ticker.tick().await;
            let sent = payload_for(seq);
            let start = Instant::now();
            if writer.write_all(&sent).await.is_err() {
                break;
            }
            if tx.send(start).await.is_err() {
                break;
            }
            seq += 1;
        }
        // Do **not** close the write half here: shutting it down now would make
        // the peer tear the stream down while replies are still in flight, and
        // the unread echoes would look like unanswered requests. Hold it open
        // until the reader has drained.
        writer_finished_flag.store(true, Ordering::Relaxed);
        while !writer_drain.load(Ordering::Relaxed) {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        let _ = writer.shutdown().await;
    });

    // Once the writer has stopped, silence for this long means every outstanding
    // reply that was coming has arrived. Comfortably above the tail the
    // diagnosis itself measures, and far above the base RTT at both scales.
    const SILENCE_GRACE: Duration = Duration::from_secs(2);
    let mut latencies = Vec::new();
    let mut buf = [0u8; MESSAGE_BYTES];
    let mut expected = 0u64;
    let read_deadline = Instant::now() + window + Duration::from_secs(10);
    loop {
        // Before the writer stops, wait as long as the arm allows; after it
        // stops, the reply stream is finite, so a short silence ends it.
        let read = if writer_finished.load(Ordering::Relaxed) {
            tokio::time::timeout(SILENCE_GRACE, reader.read_exact(&mut buf)).await
        } else {
            tokio::time::timeout_at(
                tokio::time::Instant::from_std(read_deadline),
                reader.read_exact(&mut buf),
            )
            .await
        };
        match read {
            Ok(Ok(_)) => {
                let start = match rx.recv().await {
                    Some(start) => start,
                    None => break,
                };
                let sent = payload_for(expected);
                assert_eq!(
                    &buf[..],
                    &sent[..],
                    "INSTRUMENT: cadence payload mismatch at seq {expected}"
                );
                latencies.push(start.elapsed().as_secs_f64() * 1000.0);
                expected += 1;
            }
            Ok(Err(_)) => break,
            Err(_) => break,
        }
    }
    drain_done.store(true, Ordering::Relaxed);
    let _ = tasks.join_next().await;
    // Any instant still queued with no matching reply is a request that was
    // never answered.
    let mut unanswered = 0u64;
    while rx.try_recv().is_ok() {
        unanswered += 1;
    }
    (latencies, unanswered)
}

/// `flows` concurrent round-trip flows on one hop — the multiplexed access-flow
/// shape the direct arms never send. Returns the pooled latencies across flows
/// and the total number of unanswered requests.
async fn flows_arm(target: &Target, flows: usize, window: Duration) -> (Vec<f64>, u64) {
    let mut set = JoinSet::new();
    for _ in 0..flows {
        let target = target.clone();
        set.spawn(async move { round_trip_arm(&target, window, None).await });
    }
    let mut pooled = Vec::new();
    let mut unanswered = 0u64;
    while let Some(joined) = set.join_next().await {
        let (latencies, missing) = joined.expect("a flow panicked");
        pooled.extend(latencies);
        unanswered += missing;
    }
    (pooled, unanswered)
}

/// A saturating bulk upload with the echo drained concurrently. Returns the
/// steady-state end-to-end goodput in MiB/s (the echo counter's delta across
/// `window`, sampled while the pump still runs), the bytes written, and the
/// bytes echoed back. A non-zero warmup keeps the connection ramp out of the
/// reading.
async fn bulk_arm(target: &Target, warmup: Duration, window: Duration) -> (f64, u64, u64) {
    let stream = target.connect().await;
    let (mut reader, mut writer) = tokio::io::split(stream);
    let delivered = Arc::new(AtomicU64::new(0));
    let sent = Arc::new(AtomicU64::new(0));
    let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));

    let reader_count = Arc::clone(&delivered);
    let mut tasks = JoinSet::new();
    tasks.spawn(async move {
        let mut buf = vec![0u8; 64 * 1024];
        loop {
            match reader.read(&mut buf).await {
                Ok(0) | Err(_) => return,
                Ok(n) => {
                    reader_count.fetch_add(n as u64, Ordering::Relaxed);
                }
            }
        }
    });

    let writer_count = Arc::clone(&sent);
    let writer_stop = Arc::clone(&stop);
    let drain_done = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let writer_drain = Arc::clone(&drain_done);
    let writer_finished = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let writer_finished_flag = Arc::clone(&writer_finished);
    tasks.spawn(async move {
        let chunk = vec![0x5au8; 64 * 1024];
        while !writer_stop.load(Ordering::Relaxed) {
            if writer.write_all(&chunk).await.is_err() {
                return;
            }
            writer_count.fetch_add(chunk.len() as u64, Ordering::Relaxed);
        }
        // The offered total is final only now; the flag is set after the last
        // increment so the caller cannot read a mid-flight count.
        writer_finished_flag.store(true, Ordering::Relaxed);
        // Hold the write half open until the echo of everything written has
        // been read, so the teardown cannot cut the transfer short.
        while !writer_drain.load(Ordering::Relaxed) {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let _ = writer.shutdown().await;
    });

    tokio::time::sleep(warmup).await;
    let before = delivered.load(Ordering::Relaxed);
    tokio::time::sleep(window).await;
    let after = delivered.load(Ordering::Relaxed);
    let goodput_mib_s = (after - before) as f64 / window.as_secs_f64() / (1024.0 * 1024.0);

    stop.store(true, Ordering::Relaxed);
    let settle = Instant::now() + Duration::from_secs(10);
    while !writer_finished.load(Ordering::Relaxed) && Instant::now() < settle {
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let total_sent = sent.load(Ordering::Relaxed);
    // Drain the echo of everything written, so delivery integrity is checked on
    // the whole transfer rather than on the sampled window.
    let deadline = Instant::now() + Duration::from_secs(30);
    while delivered.load(Ordering::Relaxed) < total_sent && Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    drain_done.store(true, Ordering::Relaxed);
    // The writer finishes once released; the reader is aborted, its count taken.
    while let Some(joined) = tasks.join_next().await {
        joined.expect("a bulk task panicked");
    }
    let total_delivered = delivered.load(Ordering::Relaxed);
    (goodput_mib_s, total_sent, total_delivered)
}

// ────────────────────────────── the scenario ──────────────────────────────

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Fault {
    /// An arm measures nothing.
    ZeroSamples,
    /// A request is never answered.
    Unanswered,
    /// The warm-up round trip taken before the window opens is never answered,
    /// so the steady arm's own instrument path — not the load shape — is what
    /// fails. The warm-up reports one unanswered request, and the shared guard
    /// rejects the arm (the zero-sample assertion fires first, because a warm-up
    /// that never completed produced no sample to report).
    WarmUnanswered,
    /// The arm's dial record is discarded after it ran, so an arm that measured
    /// samples reports no cold-connection reading at all. The establishment
    /// reading is what this scenario adds, so losing it must fail the arm rather
    /// than leave a table with a silent hole in it.
    Undialed,
}

fn fault_from_env() -> Option<Fault> {
    match std::env::var("PROXY_PATH_PERF_FAULT").ok().as_deref() {
        Some("zero_samples") => Some(Fault::ZeroSamples),
        Some("unanswered") => Some(Fault::Unanswered),
        Some("warm_unanswered") => Some(Fault::WarmUnanswered),
        Some("undialed") => Some(Fault::Undialed),
        _ => None,
    }
}

/// A loopback TCP front: every accepted TCP connection is relayed to a fresh
/// `rtp_mux` stream on the direct connector. This reproduces the one stage the
/// deployed chain interposes and the plain direct arm does not — the
/// application writing into a kernel TCP socket that a relay drains into the
/// mux stream — so a tail that needs this stage can be told apart from a tail
/// the proxy protocol or the transport itself causes. The relay implementation
/// is a separate dimension ([`RelayImpl`]).
async fn spawn_tcp_front(
    connector: Arc<RtpMuxConnector>,
    upstream: SocketAddr,
    relay_impl: RelayImpl,
) -> (SocketAddr, TaskScope) {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
    let addr = listener.local_addr().unwrap();
    let scope = TaskScope::new();
    let accepted = scope.clone();
    scope.spawn(async move {
        loop {
            let Ok((tcp, _)) = listener.accept().await else {
                return;
            };
            let connector = Arc::clone(&connector);
            accepted.spawn(async move {
                let Ok(stream) = connector
                    .connect_stream_with_lane(upstream, LaneClass::Interactive)
                    .await
                else {
                    return;
                };
                relay_leg(tcp, stream, relay_impl).await;
            });
        }
    });
    (addr, scope)
}

/// The matched-RTT calibration and the arm pair it makes comparable.
struct PairResult {
    proxy: ArmOutcome,
    direct: ArmOutcome,
    /// The same shape and impairment on the direct transport with a TCP front
    /// interposed; run only where it is needed to attribute a tail.
    fronted: Option<ArmOutcome>,
    /// The same shape and impairment on the direct transport whose server-side
    /// handler is a plain byte relay to the TCP echo server — the chain's
    /// server stage without the proxy protocol or the access server.
    relayed: Option<ArmOutcome>,
    /// The same shape and impairment on the direct transport with **both**
    /// stages stacked: a TCP front feeding the mux stream and a server-side
    /// handler byte-relaying that stream to the TCP echo. This is the chain
    /// minus the proxy protocol's own per-connection handling, and minus the
    /// relay wrappers the proxy's copy carries — it varies two dimensions from
    /// the direct baseline at once, so it is a **composite** and attributes
    /// only as "the stacked-relay shape as a whole, with none of the proxy's
    /// own code in it".
    front_relayed: Option<ArmOutcome>,
    /// The same stacked arm with the proxy's own relay implementation on both
    /// legs, one wrapper layer per arm: `direct_front_relay_tout` is the
    /// proxy's `TimeoutStreamShared` plus its `copy_bidirectional` fork,
    /// `direct_front_relay_timed` adds the `async_speed_limit::Limiter`. Each
    /// varies exactly one dimension from `front_relayed` (the clean control at
    /// the same topology, impairment, cadence and matched RTT): the relay
    /// implementation.
    front_relay_tout: Option<ArmOutcome>,
    front_relay_timed: Option<ArmOutcome>,
    applied_correction: Duration,
    /// The regime's calibration base RTTs (from the round-trip arm), which are
    /// what the matched-RTT check is about. A pipelined arm's own minimum is
    /// polluted by its standing queue, so it is reported but never used to
    /// claim the two links are matched.
    matched_base_proxy_ms: f64,
    matched_base_direct_ms: f64,
}

/// Which extra direct-transport controls an arm pair runs. One field per
/// stage or implementation under attribution, so a result attributes to the
/// control it names rather than to a bundled set.
#[derive(Clone, Copy, Debug, Default)]
struct Controls {
    /// The client-side kernel-TCP front alone.
    front: bool,
    /// The server-side byte relay alone.
    relay: bool,
    /// Both stages stacked: the chain minus the proxy protocol, its stream
    /// wrappers and its chain plumbing.
    front_relay: bool,
    /// The stacked topology under the proxy's own relay implementation, one
    /// wrapper layer per arm.
    relay_stack: bool,
}

/// Run a shape on both topologies in a regime, with the direct arm's one-way
/// delay corrected so the two achieved base RTTs match. The correction is
/// derived from the round-trip arm's minimum (base) RTT: the chain's extra
/// loopback hops are measured, never assumed.
async fn run_pair(
    regime: &Regime,
    shape: Shape,
    calibration: Duration,
    echo: SocketAddr,
    fault: Option<Fault>,
    controls: Controls,
) -> PairResult {
    let chain = start_chain(
        &format!("pair-{}", regime.name),
        regime,
        Duration::ZERO,
        echo,
    )
    .await;
    let proxy = run_shape(
        "proxy_chain",
        regime,
        &Target::access(chain.access_addr),
        shape,
        &chain.hop,
        fault,
    )
    .await;
    chain.shutdown().await;

    let direct_handle = start_direct(regime, calibration, echo).await;
    let direct_target = Target::direct(Arc::clone(&direct_handle.connector), direct_handle.addr);
    let direct = run_shape(
        "direct_transport",
        regime,
        &direct_target,
        shape,
        &direct_handle.hop,
        fault,
    )
    .await;
    let fronted = if controls.front {
        let (front_addr, front_scope) = spawn_tcp_front(
            Arc::clone(&direct_handle.connector),
            direct_handle.addr,
            RelayImpl::Tokio,
        )
        .await;
        let outcome = run_shape(
            "direct_tcp_front",
            regime,
            &Target::access(front_addr),
            shape,
            &direct_handle.hop,
            fault,
        )
        .await;
        front_scope.reap_ready();
        Some(outcome)
    } else {
        None
    };
    let relayed = if controls.relay {
        direct_handle
            .relay_mode
            .store(RELAY_TOKIO, Ordering::Relaxed);
        Some(
            run_shape(
                "direct_relay",
                regime,
                &direct_target,
                shape,
                &direct_handle.hop,
                fault,
            )
            .await,
        )
    } else {
        None
    };
    // The composite: the TCP front of `fronted` feeding the byte-relaying
    // server of `relayed`. Both stages are the test's own plain tokio copies,
    // so a delta here cannot be the proxy protocol, the proxy's stream
    // wrappers, the access server's chain plumbing or its connection pool.
    let front_relayed = if controls.front_relay {
        direct_handle
            .relay_mode
            .store(RELAY_TOKIO, Ordering::Relaxed);
        let (front_addr, front_scope) = spawn_tcp_front(
            Arc::clone(&direct_handle.connector),
            direct_handle.addr,
            RelayImpl::Tokio,
        )
        .await;
        let outcome = run_shape(
            "direct_front_relay",
            regime,
            &Target::access(front_addr),
            shape,
            &direct_handle.hop,
            fault,
        )
        .await;
        front_scope.reap_ready();
        Some(outcome)
    } else {
        None
    };
    // The relay-stack arms: the same stacked topology as `front_relayed`, with
    // the proxy's own relay implementation in place of the harness's plain
    // copy. `front_relayed` above is the control they are read against.
    let front_relay_tout = if controls.relay_stack {
        Some(
            run_relay_stack_arm(
                &direct_handle,
                regime,
                shape,
                fault,
                RelayImpl::ProxyTimeout,
                RELAY_PROXY_TIMEOUT,
            )
            .await,
        )
    } else {
        None
    };
    let front_relay_timed = if controls.relay_stack {
        Some(
            run_relay_stack_arm(
                &direct_handle,
                regime,
                shape,
                fault,
                RelayImpl::ProxyTimed,
                RELAY_PROXY_TIMED,
            )
            .await,
        )
    } else {
        None
    };
    direct_handle.scope.reap_ready();
    direct_handle.shutdown();

    PairResult {
        proxy,
        direct,
        fronted,
        relayed,
        front_relayed,
        front_relay_tout,
        front_relay_timed,
        applied_correction: calibration,
        matched_base_proxy_ms: 0.0,
        matched_base_direct_ms: 0.0,
    }
}

/// Run one relay-stack arm: the stacked front-plus-relay topology with the
/// proxy's relay implementation on both legs.
async fn run_relay_stack_arm(
    direct_handle: &DirectHandle,
    regime: &Regime,
    shape: Shape,
    fault: Option<Fault>,
    relay_impl: RelayImpl,
    mode: u8,
) -> ArmOutcome {
    let topology = match relay_impl {
        RelayImpl::ProxyTimeout => "direct_front_relay_tout",
        RelayImpl::ProxyTimed => "direct_front_relay_timed",
        RelayImpl::Tokio => "direct_front_relay",
    };
    direct_handle.relay_mode.store(mode, Ordering::Relaxed);
    let (front_addr, front_scope) = spawn_tcp_front(
        Arc::clone(&direct_handle.connector),
        direct_handle.addr,
        relay_impl,
    )
    .await;
    let outcome = run_shape(
        topology,
        regime,
        &Target::access(front_addr),
        shape,
        &direct_handle.hop,
        fault,
    )
    .await;
    front_scope.reap_ready();
    outcome
}

/// Run one shape against the `direct_proto` topology — the proxy binary's
/// `proxy_server` alone, entered with the harness's own protocol client — on a
/// fresh process, so its mux lane pairing is a cold one.
async fn run_proto_arm(
    regime: &Regime,
    shape: Shape,
    calibration: Duration,
    echo: SocketAddr,
    fault: Option<Fault>,
) -> ArmOutcome {
    let proto = proto_client(echo);
    let server = start_proto_server(
        &format!("proto-{}", regime.name),
        regime,
        calibration,
        &proto,
    )
    .await;
    let target = Target::proto(
        Arc::clone(&server.connector),
        server.addr,
        proto.header_crypto.clone(),
        proto.upstream.clone(),
    );
    let outcome = run_shape("direct_proto", regime, &target, shape, &server.hop, fault).await;
    server.shutdown().await;
    outcome
}

/// Assert the `direct_proto` arm's achieved base RTT matches the regime's
/// calibrated direct base. The arm adds one loopback TCP hop the direct arm
/// does not have (the `proxy_server`'s connect to the echo), but the chain has
/// that hop too and was matched against the same base, so a disagreement beyond
/// the tolerance means this arm is measuring a different path length rather
/// than the protocol's stages. Instrument sanity, not a product bound.
fn assert_proto_rtt_matched(regime: &Regime, arm: &ArmOutcome, calibrated_direct_ms: f64) {
    let delta = (arm.base_rtt_ms - calibrated_direct_ms).abs();
    assert!(
        Duration::from_secs_f64(delta / 1000.0) <= RTT_MATCH_TOL,
        "INSTRUMENT: direct_proto / {} compared at mismatched base RTT: arm {:.1} ms vs the \
         regime's calibrated direct base {:.1} ms (delta {:.1} ms > tolerance {} ms)",
        regime.name,
        arm.base_rtt_ms,
        calibrated_direct_ms,
        delta,
        RTT_MATCH_TOL.as_millis(),
    );
}

/// Measure the chain's and the direct transport's achieved base RTT, and return
/// the one-way correction that matches them together with the round-trip pair
/// it was measured on. When a correction is needed the pair is re-measured with
/// it applied, so the returned pair is always the matched one and doubles as
/// the round-trip arm's result.
async fn calibrate(regime: &Regime, echo: SocketAddr) -> (Duration, PairResult) {
    let shape = Shape::RoundTrip {
        window: Duration::from_secs(4),
    };
    let first = run_pair(
        regime,
        shape,
        Duration::ZERO,
        echo,
        None,
        Controls::default(),
    )
    .await;
    let correction_ms =
        ((first.proxy.base_rtt_ms - first.direct.base_rtt_ms) / 2.0).clamp(-150.0, 150.0);
    let correction = if correction_ms.abs() < 1.0 {
        Duration::ZERO
    } else {
        Duration::from_millis(correction_ms.round() as u64)
    };
    if correction.is_zero() {
        (correction, first)
    } else {
        let second = run_pair(regime, shape, correction, echo, None, Controls::default()).await;
        (correction, second)
    }
}

fn out_path() -> PathBuf {
    if let Ok(path) = std::env::var("PROXY_PATH_PERF_OUT") {
        return PathBuf::from(path);
    }
    let base = std::env::var("CARGO_TARGET_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../target"));
    base.join("proxy_path_perf").join("report.json")
}

/// One arm's JSON record. Shared by the pair matrix and the protocol-only
/// arms, so a field added for one is never missing from the other.
fn arm_json(arm: &ArmOutcome) -> serde_json::Value {
    let slow = slow_shape(&arm.latencies_ms);
    json!({
        "topology": arm.topology,
        "regime": arm.regime,
        "shape": arm.shape,
        "owd_ms": arm.owd_ms,
        "samples": arm.latencies_ms.len(),
        "unanswered": arm.unanswered,
        "mismatches": arm.mismatches,
        "base_rtt_ms": arm.base_rtt_ms,
        "p50_ms": arm.percentile(0.50),
        "p90_ms": arm.percentile(0.90),
        "p99_ms": arm.percentile(0.99),
        "p999_ms": arm.percentile(0.999),
        "max_ms": arm.percentile(1.0),
        "over_250ms": arm.over_ceiling(),
        "slow_first_index": slow.first,
        "slow_last_index": slow.last,
        "slow_episodes": slow.episodes,
        "slow_max_run": slow.max_run,
        "p75_ms": arm.percentile(0.75),
        "p95_ms": arm.percentile(0.95),
        "offered_bytes": arm.offered_bytes,
        "delivered_bytes": arm.delivered_bytes,
        "wire_segment": "rtpmux lanes, client->server",
        "wire_dominant_segment": if arm.wire_interactive_c2s_bytes
            >= arm.wire_bulk_c2s_bytes
        {
            "interactive"
        } else {
            "bulk"
        },
        "wire_interactive_c2s_bytes": arm.wire_interactive_c2s_bytes,
        "wire_interactive_s2c_bytes": arm.wire_interactive_s2c_bytes,
        "wire_bulk_lane_c2s_bytes": arm.wire_bulk_c2s_bytes,
        "wire_multiple_interactive": arm.wire_multiple(),
        "wire_multiple_dominant_lane": if arm.offered_bytes == 0 {
            f64::NAN
        } else {
            arm.wire_interactive_c2s_bytes.max(arm.wire_bulk_c2s_bytes) as f64
                / arm.offered_bytes as f64
        },
        "netem_interactive_forwarded_events": arm.netem_forwarded,
        "netem_interactive_dropped_events": arm.netem_dropped,
        "netem_interactive_delayed_events": arm.netem_delayed,
        "goodput_mib_s": arm.goodput_mib_s,
        "echo_elapsed_s": arm.echo_elapsed_s,
        "dials": arm.dials,
        "connect_ms": arm.connect_ms,
        "mux_dial_ms": arm.mux_dial_ms,
        "protocol_ms": arm.protocol_ms,
        "mux_session_reused": arm.mux_session_reused,
        "first_ms": arm.first_ms,
        "cold_total_ms": arm.cold_total_ms,
    })
}

fn record_json(results: &[PairResult], proto_arms: &[ArmOutcome]) -> serde_json::Value {
    let arms: Vec<serde_json::Value> = results
        .iter()
        .flat_map(|pair| {
            [
                Some(&pair.proxy),
                Some(&pair.direct),
                pair.fronted.as_ref(),
                pair.relayed.as_ref(),
                pair.front_relayed.as_ref(),
                pair.front_relay_tout.as_ref(),
                pair.front_relay_timed.as_ref(),
            ]
            .into_iter()
            .flatten()
        })
        .chain(proto_arms.iter())
        .map(arm_json)
        .collect();

    let deltas: Vec<serde_json::Value> = results
        .iter()
        .map(|pair| {
            json!({
                "regime": pair.proxy.regime,
                "shape": pair.proxy.shape,
                "applied_direct_owd_correction_ms": pair.applied_correction.as_millis() as u64,
                "regime_matched_base_proxy_ms": pair.matched_base_proxy_ms,
                "regime_matched_base_direct_ms": pair.matched_base_direct_ms,
                "regime_matched_base_delta_ms": pair.matched_base_proxy_ms
                    - pair.matched_base_direct_ms,
                "proxy_base_rtt_ms": pair.proxy.base_rtt_ms,
                "direct_base_rtt_ms": pair.direct.base_rtt_ms,
                "matched_rtt_delta_ms": pair.matched_base_proxy_ms
                    - pair.matched_base_direct_ms,
                "proxy_minus_direct_p50_ms": pair.proxy.percentile(0.50) - pair.direct.percentile(0.50),
                "proxy_minus_direct_p99_ms": pair.proxy.percentile(0.99) - pair.direct.percentile(0.99),
                "proxy_minus_direct_p999_ms": pair.proxy.percentile(0.999) - pair.direct.percentile(0.999),
                "proxy_minus_direct_max_ms": pair.proxy.percentile(1.0) - pair.direct.percentile(1.0),
                "proxy_over_250ms": pair.proxy.over_ceiling(),
                "direct_over_250ms": pair.direct.over_ceiling(),
                "fronted_p99_ms": pair.fronted.as_ref().map(|f| f.percentile(0.99)),
                "fronted_over_250ms": pair.fronted.as_ref().map(|f| f.over_ceiling()),
                "proxy_minus_fronted_p99_ms": pair
                    .fronted
                    .as_ref()
                    .map(|f| pair.proxy.percentile(0.99) - f.percentile(0.99)),
                "relayed_p99_ms": pair.relayed.as_ref().map(|r| r.percentile(0.99)),
                "relayed_over_250ms": pair.relayed.as_ref().map(|r| r.over_ceiling()),
                "relayed_minus_direct_p99_ms": pair
                    .relayed
                    .as_ref()
                    .map(|r| r.percentile(0.99) - pair.direct.percentile(0.99)),
                "proxy_minus_relayed_p99_ms": pair
                    .relayed
                    .as_ref()
                    .map(|r| pair.proxy.percentile(0.99) - r.percentile(0.99)),
                "front_relayed_p99_ms": pair.front_relayed.as_ref().map(|f| f.percentile(0.99)),
                "front_relayed_over_250ms": pair.front_relayed.as_ref().map(|f| f.over_ceiling()),
                "front_relayed_minus_direct_p99_ms": pair
                    .front_relayed
                    .as_ref()
                    .map(|f| f.percentile(0.99) - pair.direct.percentile(0.99)),
                "front_relay_timed_over_250ms": pair
                    .front_relay_timed
                    .as_ref()
                    .map(|f| f.over_ceiling()),
                "front_relay_tout_p99_ms": pair
                    .front_relay_tout
                    .as_ref()
                    .map(|f| f.percentile(0.99)),
                "front_relay_tout_over_250ms": pair
                    .front_relay_tout
                    .as_ref()
                    .map(|f| f.over_ceiling()),
                "front_relay_timed_p99_ms": pair
                    .front_relay_timed
                    .as_ref()
                    .map(|f| f.percentile(0.99)),
                "proxy_minus_front_relay_tout_p99_ms": pair
                    .front_relay_tout
                    .as_ref()
                    .map(|f| pair.proxy.percentile(0.99) - f.percentile(0.99)),
                "proxy_minus_front_relay_timed_p99_ms": pair
                    .front_relay_timed
                    .as_ref()
                    .map(|f| pair.proxy.percentile(0.99) - f.percentile(0.99)),
            })
        })
        .collect();

    json!({
        "scenario": "proxy_path_perf",
        "kind": "diagnosis",
        "bounds_authority": "crates/rtp_mux/GATE.md §Performance (not restated here)",
        "binary": env!("CARGO_BIN_EXE_proxy"),
        "emulated_bulk_shaped_rate_bps": BULK_CAPACITY_BPS,
        "effective_capacity_note": "the direct arm's own measured bulk goodput is the effective \
            capacity; no fraction is computed against the shaped rate because neither arm \
            reaches it",
        "rtt_match_tolerance_ms": RTT_MATCH_TOL.as_millis() as u64,
        "cold_connection_note": "connect_ms + first_ms is the same clock on every topology: \
            the client's first act of connecting (a TCP accept for the chain, the rtp_mux lane \
            pairing for the direct arms, the pairing plus the proxy protocol for direct_proto) \
            to its first echo. mux_dial_ms and protocol_ms are the parts of connect_ms the arm's \
            own topology performs; the chain's parts run inside the binary, after its window \
            opened, and so appear in its first sample instead.",
        "arms": arms,
        "proto_arms": proto_arms.iter().map(arm_json).collect::<Vec<_>>(),
        "deltas": deltas,
    })
}

/// One arm's row in the per-arm table, plus its slow-sample shape line when it
/// has a tail.
fn print_arm(arm: &ArmOutcome) {
    // Report the wire against the lane that actually carried the arm: the
    // interactive lane for the interactive shapes, the bulk lane once a bulk
    // flow has migrated onto it.
    let dominant = arm.wire_interactive_c2s_bytes.max(arm.wire_bulk_c2s_bytes);
    let multiple = if arm.offered_bytes == 0 {
        f64::NAN
    } else {
        dominant as f64 / arm.offered_bytes as f64
    };
    println!(
        "{:<16} {:<10} {:<8} {:>7.1} {:>7.1} {:>8.1} {:>8.1} {:>8.1} {:>9} {:>8} {:>7} {:>6.2} {:>6}",
        arm.topology,
        arm.regime,
        arm.shape,
        arm.base_rtt_ms,
        arm.percentile(0.50),
        arm.percentile(0.99),
        arm.percentile(0.999),
        arm.percentile(1.0),
        arm.over_ceiling(),
        arm.latencies_ms.len(),
        arm.unanswered,
        multiple,
        arm.netem_dropped,
    );
    let slow = slow_shape(&arm.latencies_ms);
    if slow.count > 0 {
        println!(
            "  slow {}: n={} first_idx={} last_idx={} episodes={} max_run={} \
             (250 ms ceiling, emission order)",
            arm.label(),
            slow.count,
            slow.first.unwrap_or(0),
            slow.last.unwrap_or(0),
            slow.episodes,
            slow.max_run,
        );
    }
}

/// The cold-connection measurement, one row per topology per regime: what
/// `connect()` cost, which parts the arm's own topology performed, how long the
/// first echo then took, and the sum — the same clock on every topology.
///
/// `mux_dial_ms` is the `rtp_mux` lane pairing and `protocol_ms` the proxy
/// protocol (flow kind, preamble, relay header); a topology that performs a
/// part inside its own process shows it in `connect`, while the chain's parts
/// run inside the binary after its window opened and so appear in `first`.
/// `reused` marks a dial that found a live mux session, which is not a
/// cold-connection reading.
fn print_cold_table(results: &[PairResult], proto_arms: &[ArmOutcome]) {
    println!("\n=== cold connection: connect() + first echo, request/response shape (ms) ===");
    println!(
        "{:<10} {:<17} {:>9} {:>10} {:>9} {:>8} {:>11} {:>7} {:>9}",
        "regime",
        "topology",
        "connect",
        "mux_dial",
        "protocol",
        "first",
        "cold_total",
        "reused",
        "dials"
    );
    let cell = |arm: &ArmOutcome| {
        let ms = |v: Option<f64>| match v {
            Some(v) => format!("{v:.1}"),
            None => "-".to_string(),
        };
        println!(
            "{:<10} {:<17} {:>9} {:>10} {:>9} {:>8} {:>11} {:>7} {:>9}",
            arm.regime,
            arm.topology,
            ms(arm.connect_ms),
            ms(arm.mux_dial_ms),
            ms(arm.protocol_ms),
            ms(arm.first_ms),
            ms(arm.cold_total_ms),
            if arm.mux_session_reused {
                "reused"
            } else {
                "no"
            },
            arm.dials,
        );
    };
    // Only the request/response arms: the pipelined shapes write several
    // requests into the window before the first reply returns, so their first
    // sample carries a standing queue as well as the establishment, and the
    // calibration pair's round-trip arms are the cold reading of the other two
    // topologies.
    for pair in results {
        if pair.proxy.shape != "rr" {
            continue;
        }
        cell(&pair.proxy);
        cell(&pair.direct);
    }
    for arm in proto_arms {
        if arm.shape != "rr" {
            continue;
        }
        cell(arm);
    }
}

fn print_table(results: &[PairResult], proto_arms: &[ArmOutcome]) {
    println!("\n=== proxy-path diagnosis: per-arm measurements ===");
    println!(
        "{:<16} {:<10} {:<8} {:>7} {:>7} {:>8} {:>8} {:>8} {:>9} {:>8} {:>7} {:>6} {:>6}",
        "topology",
        "regime",
        "shape",
        "base",
        "p50",
        "p99",
        "p99.9",
        "max",
        ">250ms",
        "n",
        "unans",
        "wire",
        "drop_ev"
    );
    for pair in results {
        for arm in [
            Some(&pair.proxy),
            Some(&pair.direct),
            pair.fronted.as_ref(),
            pair.relayed.as_ref(),
            pair.front_relayed.as_ref(),
            pair.front_relay_tout.as_ref(),
            pair.front_relay_timed.as_ref(),
        ]
        .into_iter()
        .flatten()
        {
            print_arm(arm);
        }
        if let Some(goodput) = pair.proxy.goodput_mib_s {
            let direct = pair.direct.goodput_mib_s.unwrap_or(f64::NAN);
            println!(
                "  bulk: proxy {:.3} MiB/s vs direct {:.3} MiB/s (effective capacity = direct; \
                 shaped rate {:.3} MiB/s, reached by neither) => proxy = {:.3}x direct",
                goodput,
                direct,
                BULK_CAPACITY_BPS as f64 / 8.0 / (1024.0 * 1024.0),
                goodput / direct,
            );
        }
    }
    // The protocol-only arms are one per topology and regime, so they print
    // after the pair matrix rather than once per pair.
    for arm in proto_arms {
        print_arm(arm);
    }
    println!("\n=== matched-RTT delta: proxy - direct (ms) ===");
    println!(
        "{:<10} {:<8} {:>9} {:>9} {:>8} {:>9} {:>9} {:>9} {:>9} {:>9} {:>9}",
        "regime",
        "shape",
        "cal_P",
        "cal_D",
        "corr",
        "d_p50",
        "d_p99",
        "d_p99.9",
        "p99vfront",
        "p99vrelay",
        "p99vfr"
    );
    for pair in results {
        let fronted = match &pair.fronted {
            Some(fronted) => {
                format!(
                    "{:>9.1}",
                    pair.proxy.percentile(0.99) - fronted.percentile(0.99)
                )
            }
            None => "        -".to_string(),
        };
        let relayed = match &pair.relayed {
            Some(relayed) => {
                format!(
                    "{:>9.1}",
                    pair.proxy.percentile(0.99) - relayed.percentile(0.99)
                )
            }
            None => "        -".to_string(),
        };
        let front_relayed = match &pair.front_relayed {
            Some(front_relayed) => {
                format!(
                    "{:>9.1}",
                    pair.proxy.percentile(0.99) - front_relayed.percentile(0.99)
                )
            }
            None => "        -".to_string(),
        };
        println!(
            "{:<10} {:<8} {:>9.1} {:>9.1} {:>8} {:>9.1} {:>9.1} {:>9.1} {:>9} {:>9} {:>9}",
            pair.proxy.regime,
            pair.proxy.shape,
            pair.matched_base_proxy_ms,
            pair.matched_base_direct_ms,
            pair.applied_correction.as_millis(),
            pair.proxy.percentile(0.50) - pair.direct.percentile(0.50),
            pair.proxy.percentile(0.99) - pair.direct.percentile(0.99),
            pair.proxy.percentile(0.999) - pair.direct.percentile(0.999),
            fronted,
            relayed,
            front_relayed,
        );
    }
    print_relay_stack(results);
    println!();
}

/// The relay-stack arms side by side: the chain, the clean stacked control, and
/// the same stack under the proxy's own relay implementation one wrapper layer
/// at a time. This is the table that attributes a delta to the relay, so it
/// prints the control's own row from the same run rather than a remembered
/// number.
fn print_relay_stack(results: &[PairResult]) {
    println!("\n=== relay-stack arms: p99 ms (over-250 ms count) ===");
    println!(
        "{:<10} {:<8} {:>18} {:>16} {:>16} {:>18} {:>18}",
        "regime", "shape", "proxy_chain", "direct", "fr/tokio", "fr/proxy_tout", "fr/proxy_timed"
    );
    let cell = |arm: Option<&ArmOutcome>| match arm {
        Some(arm) => format!("{:.1} ({})", arm.percentile(0.99), arm.over_ceiling()),
        None => "-".to_string(),
    };
    for pair in results {
        if pair.front_relay_tout.is_none() && pair.front_relay_timed.is_none() {
            continue;
        }
        println!(
            "{:<10} {:<8} {:>18} {:>16} {:>16} {:>18} {:>18}",
            pair.proxy.regime,
            pair.proxy.shape,
            cell(Some(&pair.proxy)),
            cell(Some(&pair.direct)),
            cell(pair.front_relayed.as_ref()),
            cell(pair.front_relay_tout.as_ref()),
            cell(pair.front_relay_timed.as_ref()),
        );
    }
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "diagnosis: drives the real proxy binary under seeded impairment; opt-in, see GATE.md"]
async fn proxy_path_matched_rtt_delta() {
    let fault = fault_from_env();
    let (echo, echo_scope) = spawn_tcp_echo().await;
    let mut results: Vec<PairResult> = Vec::new();
    let mut proto_arms: Vec<ArmOutcome> = Vec::new();

    // The fault runs a reduced matrix: the guard it exercises is shared with
    // every arm, so a cheap single pair demonstrates the vacuity.
    let interactive_window = if fault.is_some() {
        Duration::from_secs(2)
    } else {
        Duration::from_secs(4)
    };
    let regimes: Vec<Regime> = if fault.is_some() {
        vec![Regime::clean25()]
    } else {
        vec![Regime::clean25(), Regime::jitter25(), Regime::field100()]
    };

    for regime in &regimes {
        let (correction, mut cal_pair) = calibrate(regime, echo).await;
        let matched_proxy_base = cal_pair.proxy.base_rtt_ms;
        let matched_direct_base = cal_pair.direct.base_rtt_ms;
        println!(
            "CALIBRATION regime={} correction_ms={} proxy_base={:.1} direct_base={:.1}",
            regime.name,
            correction.as_millis(),
            cal_pair.proxy.base_rtt_ms,
            cal_pair.direct.base_rtt_ms
        );
        if fault.is_none() {
            // The calibration pair is the round-trip arm: re-measuring the same
            // shape would only spend another window.
            cal_pair.proxy.assert_sane();
            cal_pair.direct.assert_sane();
            assert_matched_rtt(&cal_pair);
            cal_pair.matched_base_proxy_ms = matched_proxy_base;
            cal_pair.matched_base_direct_ms = matched_direct_base;
            results.push(cal_pair);
        }

        let mut shapes = vec![
            Shape::RoundTrip {
                window: interactive_window,
            },
            Shape::Cadence {
                window: interactive_window,
                interval: Duration::from_millis(5),
            },
        ];
        // The steady-state reading of the same cadence, at the two 25 ms-OWD
        // scales: one warm round trip first, so the reading is the established
        // path. The direct arms' `connect()` already establishes the mux
        // stream, so without this the two topologies' windows start at
        // different points in their connection lifecycle.
        if (fault.is_none() || fault == Some(Fault::WarmUnanswered)) && regime.name != "field100" {
            shapes.push(Shape::CadenceSteady {
                window: interactive_window,
                interval: Duration::from_millis(5),
            });
        }
        for shape in shapes {
            if fault.is_none() && matches!(shape, Shape::RoundTrip { .. }) {
                continue;
            }
            // The control arms run only on the plain pipelined cadence at the
            // two 25 ms-OWD scales — the shape and the scale the main
            // comparison finds a delta at — so the attribution costs one run
            // per control rather than one per arm of the matrix. The steady
            // arm carries the same already-established path in both
            // topologies, so it needs no control: its proxy-minus-direct delta
            // is the steady-state comparison by construction.
            let cadence = fault.is_none() && matches!(shape, Shape::Cadence { .. });
            let not_field_scale = regime.name != "field100";
            let controls = Controls {
                front: cadence,
                relay: cadence && not_field_scale,
                front_relay: cadence,
                relay_stack: cadence && not_field_scale,
            };
            let pair = run_pair(regime, shape, correction, echo, fault, controls).await;
            pair.proxy.assert_sane();
            pair.direct.assert_sane();
            if let Some(fronted) = &pair.fronted {
                fronted.assert_sane();
            }
            if let Some(relayed) = &pair.relayed {
                relayed.assert_sane();
            }
            if let Some(front_relayed) = &pair.front_relayed {
                front_relayed.assert_sane();
            }
            if let Some(front_relay_tout) = &pair.front_relay_tout {
                front_relay_tout.assert_sane();
            }
            if let Some(front_relay_timed) = &pair.front_relay_timed {
                front_relay_timed.assert_sane();
            }
            // The matched-RTT claim belongs to the regime's calibration, not to
            // this shape's own minimum: a queued pipelined arm has no clean
            // floor, and reporting its minimum as a "base RTT" would confuse a
            // standing queue with a longer link.
            let mut pair = pair;
            pair.matched_base_proxy_ms = matched_proxy_base;
            pair.matched_base_direct_ms = matched_direct_base;
            results.push(pair);
        }

        // The proxy-protocol arm: the same binary's `proxy_server`, entered by
        // the harness's own protocol client, so the chain's ingress stage (the
        // TCP accept, the chain selection, the pooled connect at the access
        // server) is the one thing it does not have. It runs the request /
        // response shape the cold-connection charge is measured on and the
        // pipelined shape the charge was found in, each on its own fresh
        // process so the lane pairing is cold.
        if fault.is_none() {
            for shape in [
                Shape::RoundTrip {
                    window: interactive_window,
                },
                Shape::Cadence {
                    window: interactive_window,
                    interval: Duration::from_millis(5),
                },
            ] {
                let outcome = run_proto_arm(regime, shape, correction, echo, fault).await;
                outcome.assert_sane();
                if matches!(shape, Shape::RoundTrip { .. }) {
                    assert_proto_rtt_matched(regime, &outcome, matched_direct_base);
                }
                proto_arms.push(outcome);
            }
        }

        // The multiplexed access-flow shape is the shape question; it is asked
        // at one RTT scale rather than at every scale.
        if fault.is_none() && regime.name == "clean25" {
            let pair = run_pair(
                regime,
                Shape::Flows {
                    flows: 4,
                    window: interactive_window,
                },
                correction,
                echo,
                None,
                Controls::default(),
            )
            .await;
            pair.proxy.assert_sane();
            pair.direct.assert_sane();
            let mut pair = pair;
            pair.matched_base_proxy_ms = matched_proxy_base;
            pair.matched_base_direct_ms = matched_direct_base;
            results.push(pair);
        }
    }

    // The shaped regime differs from clean25 only by the rate shaper, so its
    // base RTT is already matched by clean25's calibration and a second
    // calibration pair would only spend another window.
    let clean25_matched = results
        .iter()
        .find(|pair| pair.proxy.regime == Regime::clean25().name)
        .map(|pair| (pair.matched_base_proxy_ms, pair.matched_base_direct_ms))
        .unwrap_or((0.0, 0.0));

    if fault.is_none() {
        let shaped = Regime::clean25_shaped();
        let pair = run_pair(
            &shaped,
            Shape::Bulk {
                warmup: BULK_WARMUP,
                window: BULK_WINDOW,
            },
            Duration::ZERO,
            echo,
            None,
            Controls::default(),
        )
        .await;
        pair.proxy.assert_sane();
        pair.direct.assert_sane();
        // Instrument sanity, not a product bound: the direct arm's own measured
        // goodput is the effective capacity, and the relay is reported as a
        // fraction of *that*. A fraction of the shaped rate would be vacuous,
        // because neither arm reaches it.
        let direct_goodput = pair.direct.goodput_mib_s.unwrap_or(0.0);
        let shaped_mib_s = BULK_CAPACITY_BPS as f64 / 8.0 / (1024.0 * 1024.0);
        assert!(
            direct_goodput >= DIRECT_BULK_SATURATION * shaped_mib_s,
            "INSTRUMENT: the direct arm carried only {direct_goodput:.3} MiB/s over a link shaped \
             at {shaped_mib_s:.3} MiB/s, so the bulk comparison is not measuring bulk traffic"
        );
        let mut pair = pair;
        pair.matched_base_proxy_ms = clean25_matched.0;
        pair.matched_base_direct_ms = clean25_matched.1;
        results.push(pair);
    }

    print_table(&results, &proto_arms);
    print_cold_table(&results, &proto_arms);
    echo_scope.reap_ready();
    let report = record_json(&results, &proto_arms);
    let path = out_path();
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, serde_json::to_vec_pretty(&report).unwrap()).unwrap();
    println!("JSON written to {}", path.display());
}

/// Assert the two topologies were actually compared at matched end-to-end RTT.
/// This is an assertion about the instrument, not about the product.
fn assert_matched_rtt(pair: &PairResult) {
    let delta = (pair.proxy.base_rtt_ms - pair.direct.base_rtt_ms).abs();
    assert!(
        Duration::from_secs_f64(delta / 1000.0) <= RTT_MATCH_TOL,
        "INSTRUMENT: {} / {} compared at mismatched base RTT: proxy {:.1} ms vs direct {:.1} ms \
         (delta {:.1} ms > tolerance {} ms), so the delta below would measure path length, not \
         the proxy layer",
        pair.proxy.regime,
        pair.proxy.shape,
        pair.proxy.base_rtt_ms,
        pair.direct.base_rtt_ms,
        delta,
        RTT_MATCH_TOL.as_millis(),
    );
}
