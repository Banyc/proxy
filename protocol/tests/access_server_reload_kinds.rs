//! Pins the scope of an access-server reload commit across its listener
//! *kinds*.
//!
//! `AccessServerLoader::commit` commits five independent kinds — tcp, udp,
//! http, socks5/tcp and socks5/udp — each its own `common::loading::Loader`
//! with its own handle map and its own prepared ops. The kinds share no live
//! state, so a listener that dies in an earlier kind must not suppress a kind
//! whose listeners are healthy and whose commit cannot fail. Every kind is
//! attempted and the failures are reported together, each named.
//!
//! This test drives the production `access_server::prepare` /
//! `AccessServerLoader::commit` entry points with a scripted config and
//! test-owned loopback listeners, and reads which destination served a
//! session off a per-session token rather than off timing:
//!
//! - generation 1 installs a `tcp_server` listener whose task the test owns;
//! - generation 2 adds an `http_server` listener, so that a later generation
//!   can remove it;
//! - generation 3 re-points the tcp listener (its task is aborted between
//!   prepare and commit, so the tcp kind fails) and *adds* a `udp_server`
//!   listener while *removing* the http one. With the fix the udp listener
//!   spawns and serves its own destination, and the removed http listener
//!   stops serving; without it the udp kind is never committed and the http
//!   kind is never committed either;
//! - generation 4 commits the same config again: the tcp listener the failed
//!   commit left dead is re-spawned and serves the destination generation 3
//!   named.
//!
//! Listeners bind `127.0.0.1:0`; each generation that spawns exactly one new
//! listener is read back from that listener's own `Listening` event, so no
//! port is ever raced. Every background task lives in the test's `JoinSet`,
//! which aborts the lot when the test returns.

use std::{
    collections::HashMap,
    io,
    net::SocketAddr,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};

use ae::anti_replay::{ReplayValidator, TimeValidator};
use common::{
    anti_replay::{VALIDATOR_CAPACITY, VALIDATOR_TIME_FRAME, VALIDATOR_UDP_HDR_TTL},
    connect::{ConnectorConfig, ConnectorResetSignal, connector_config_cell},
    error::AnyError,
    lifecycle::retention::{RetentionActor, RetentionActorSender},
    notify::Notify,
    proxy_runtime::{
        connect::udp::UdpConnector,
        context::{Runtime, StreamRuntime, UdpRuntime},
    },
    route::HopConfig,
    session::SessionSpawner,
    stream_runtime::pool::StreamConnPool,
};
use protocol::{
    access_server::{self, AccessServerConfig, AccessServerLoader},
    stream_proto::connect::build_concrete_stream_connector_table,
};
use swap::Swap;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream, UdpSocket},
    task::JoinSet,
};
use tokio_util::sync::CancellationToken;

/// Sessions relay a fixed-size token and the destination echoes it back
/// prefixed with `E:`, so a relay is byte-exact and which destination served a
/// session is read off the token's identity, never off timing.
const TOKEN_LEN: usize = 16;

/// Every await that waits for a commit effect (a listener starting, a socket
/// closing, a datagram returning) is bounded by this. A budget that expires is
/// reported with the phase that was waiting, so a stall names itself.
const BUDGET: Duration = Duration::from_secs(10);

/// The message the serve loop logs when it starts a listener, carrying the
/// address it bound in an `addr` field.
const LISTEN_MESSAGE: &str = "Listening";

/// The test's tasks: the process actors and destinations live here, so
/// returning from the test aborts them.
type Tasks = JoinSet<()>;

/// The server-task set `commit` spawns listener tasks into.
type ServerTasks = JoinSet<Result<(), AnyError>>;

/// A distinct token per session, so a destination that read it can be told
/// apart from every other destination.
fn token(phase: u8, index: u8) -> [u8; TOKEN_LEN] {
    let mut token = [0u8; TOKEN_LEN];
    token[0] = b'T';
    token[1] = phase;
    token[2] = index;
    token
}

// -- the captured log ----------------------------------------------------------------

#[derive(Default)]
struct CaptureVisitor {
    parts: Vec<String>,
}
impl tracing::field::Visit for CaptureVisitor {
    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        self.parts.push(format!("{}={value:?}", field.name()));
    }
    fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
        self.parts.push(format!("{}={value}", field.name()));
    }
    fn record_i64(&mut self, field: &tracing::field::Field, value: i64) {
        self.parts.push(format!("{}={value}", field.name()));
    }
    fn record_u64(&mut self, field: &tracing::field::Field, value: u64) {
        self.parts.push(format!("{}={value}", field.name()));
    }
    fn record_bool(&mut self, field: &tracing::field::Field, value: bool) {
        self.parts.push(format!("{}={value}", field.name()));
    }
    fn record_f64(&mut self, field: &tracing::field::Field, value: f64) {
        self.parts.push(format!("{}={value}", field.name()));
    }
}

/// Renders every event's target and fields into a buffer, so the test reads a
/// listener's own `Listening` event for the address it bound. Process-global:
/// listeners run on the runtime's threads, not the test's.
struct CaptureSubscriber {
    buf: Arc<Mutex<Vec<u8>>>,
}
impl tracing::Subscriber for CaptureSubscriber {
    fn enabled(&self, _metadata: &tracing::Metadata<'_>) -> bool {
        true
    }
    fn new_span(&self, _span: &tracing::span::Attributes<'_>) -> tracing::span::Id {
        tracing::span::Id::from_u64(1)
    }
    fn record(&self, _span: &tracing::span::Id, _values: &tracing::span::Record<'_>) {}
    fn record_follows_from(&self, _span: &tracing::span::Id, _follows: &tracing::span::Id) {}
    fn event(&self, event: &tracing::Event<'_>) {
        let mut visitor = CaptureVisitor::default();
        event.record(&mut visitor);
        let mut buf = self.buf.lock().unwrap();
        buf.extend_from_slice(
            format!(
                "[{}] {}\n",
                event.metadata().target(),
                visitor.parts.join(" ")
            )
            .as_bytes(),
        );
    }
    fn enter(&self, _span: &tracing::span::Id) {}
    fn exit(&self, _span: &tracing::span::Id) {}
}

/// Install the capture subscriber and hand back its buffer. This file holds
/// exactly one test, so no concurrent event can displace the subscriber and
/// no other test's listener can be mistaken for one of this test's.
fn capture_events() -> Arc<Mutex<Vec<u8>>> {
    let buf: Arc<Mutex<Vec<u8>>> = Arc::default();
    tracing::subscriber::set_global_default(CaptureSubscriber {
        buf: Arc::clone(&buf),
    })
    .expect("no subscriber is installed before this test");
    buf
}

/// Every address a listener has reported through its `Listening` event, in the
/// order the events landed. The listener that bound the socket is the one that
/// logs it, so the address is what it actually bound.
fn listen_addrs(buf: &Arc<Mutex<Vec<u8>>>) -> Vec<String> {
    String::from_utf8(buf.lock().unwrap().clone())
        .expect("every event renders as UTF-8")
        .lines()
        .filter(|line| line.contains(LISTEN_MESSAGE))
        .filter_map(|line| line.split("addr=").nth(1))
        .filter_map(|rest| rest.split_whitespace().next())
        .map(|addr| addr.trim_end_matches('"').to_owned())
        .filter(|addr| !addr.is_empty())
        .collect()
}

/// Wait, bounded, until `want` listeners have reported their address, and
/// return the newest one. A listener logs on its first poll, which the commit
/// schedules, so its absence after a commit that should have spawned it is a
/// failure and not a timer.
async fn await_new_listen_addr(buf: &Arc<Mutex<Vec<u8>>>, want: usize, phase: &str) -> String {
    let deadline = Instant::now() + BUDGET;
    loop {
        let addrs = listen_addrs(buf);
        if addrs.len() >= want {
            return addrs.last().expect("at least `want` addresses").clone();
        }
        assert!(
            Instant::now() < deadline,
            "{phase}: only {} listener(s) reported an address within {BUDGET:?}; expected \
             {want}. A listener a commit spawned logs its address on its first poll, so one \
             that never reports has not been served.",
            addrs.len()
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

// -- destinations ---------------------------------------------------------------------

/// A test-owned TCP origin: it counts accepted connections, records every
/// token it reads, and echoes each one back prefixed with `E:`.
struct TcpDestination {
    port: u16,
    accepts: Arc<AtomicUsize>,
    received: Arc<Mutex<Vec<[u8; TOKEN_LEN]>>>,
    _tasks: JoinSet<()>,
}

impl TcpDestination {
    async fn bind() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("an ephemeral loopback port must be available");
        let port = listener
            .local_addr()
            .expect("a bound listener has a local address")
            .port();
        let accepts = Arc::new(AtomicUsize::new(0));
        let received = Arc::new(Mutex::new(Vec::new()));
        let mut tasks = JoinSet::new();
        {
            let accepts = Arc::clone(&accepts);
            let received = Arc::clone(&received);
            tasks.spawn(async move {
                let mut handlers: JoinSet<()> = JoinSet::new();
                loop {
                    tokio::select! {
                        accepted = listener.accept() => match accepted {
                            Ok((stream, _peer)) => {
                                accepts.fetch_add(1, Ordering::SeqCst);
                                handlers.spawn(echo(stream, Arc::clone(&received)));
                            }
                            Err(_) => return,
                        },
                        Some(_) = handlers.join_next() => {}
                    }
                }
            });
        }
        Self {
            port,
            accepts,
            received,
            _tasks: tasks,
        }
    }

    fn accepts(&self) -> usize {
        self.accepts.load(Ordering::SeqCst)
    }

    fn saw(&self, token: &[u8; TOKEN_LEN]) -> bool {
        self.received.lock().unwrap().contains(token)
    }
}

/// Read fixed-size tokens on one connection and echo each one. Any read or
/// write error ends the handler, so a destination never panics on a client
/// that gave up.
async fn echo(mut stream: TcpStream, received: Arc<Mutex<Vec<[u8; TOKEN_LEN]>>>) {
    loop {
        let mut token = [0u8; TOKEN_LEN];
        if stream.read_exact(&mut token).await.is_err() {
            return;
        }
        received.lock().unwrap().push(token);
        let mut echoed = [0u8; TOKEN_LEN + 2];
        echoed[..2].copy_from_slice(b"E:");
        echoed[2..].copy_from_slice(&token);
        if stream.write_all(&echoed).await.is_err() {
            return;
        }
    }
}

/// A test-owned UDP origin: it records every token it reads and echoes each
/// one back prefixed with `E:`, so a routed datagram and its reply are both
/// byte-identifiable.
struct UdpDestination {
    addr: SocketAddr,
    received: Arc<Mutex<Vec<[u8; TOKEN_LEN]>>>,
    _tasks: JoinSet<()>,
}

impl UdpDestination {
    async fn bind() -> Self {
        let socket = UdpSocket::bind("127.0.0.1:0")
            .await
            .expect("an ephemeral loopback port must be available");
        let addr = socket
            .local_addr()
            .expect("a bound socket has a local address");
        let received = Arc::new(Mutex::new(Vec::new()));
        let mut tasks = JoinSet::new();
        {
            let received = Arc::clone(&received);
            tasks.spawn(async move {
                let mut buf = [0u8; 2048];
                loop {
                    let Ok((len, from)) = socket.recv_from(&mut buf).await else {
                        return;
                    };
                    if len != TOKEN_LEN {
                        continue;
                    }
                    let mut token = [0u8; TOKEN_LEN];
                    token.copy_from_slice(&buf[..TOKEN_LEN]);
                    received.lock().unwrap().push(token);
                    let mut echoed = [0u8; TOKEN_LEN + 2];
                    echoed[..2].copy_from_slice(b"E:");
                    echoed[2..].copy_from_slice(&token);
                    let _ = socket.send_to(&echoed, from).await;
                }
            });
        }
        Self {
            addr,
            received,
            _tasks: tasks,
        }
    }

    fn saw(&self, token: &[u8; TOKEN_LEN]) -> bool {
        self.received.lock().unwrap().contains(token)
    }
}

// -- clients --------------------------------------------------------------------------

/// Open a session through a TCP listener at `addr`, send `token`, and require
/// its echo. Success means the bytes reached a destination and came back, so
/// the caller attributes the session by which destination recorded the token.
async fn relay_tcp(addr: &str, destination: &TcpDestination, token: [u8; TOKEN_LEN]) {
    let mut stream = tokio::time::timeout(BUDGET, TcpStream::connect(addr))
        .await
        .unwrap_or_else(|_| panic!("connecting to {addr} timed out"))
        .unwrap_or_else(|e| panic!("a session through {addr} must be accepted: {e}"));
    let mut expected = [0u8; TOKEN_LEN + 2];
    expected[..2].copy_from_slice(b"E:");
    expected[2..].copy_from_slice(&token);
    tokio::time::timeout(BUDGET, async {
        stream.write_all(&token).await?;
        let mut got = [0u8; TOKEN_LEN + 2];
        stream.read_exact(&mut got).await?;
        if got != expected {
            return Err(io::Error::other("the echo did not match the token"));
        }
        Ok(())
    })
    .await
    .unwrap_or_else(|_| panic!("a session through {addr} timed out"))
    .unwrap_or_else(|e| panic!("a session through {addr} must relay: {e}"));
    assert!(
        destination.saw(&token),
        "the session through {addr} relayed, but that destination never read the token: the \
         relay reached some other destination"
    );
}

/// Send `token` through a UDP listener at `addr` and require the destination
/// to record it and to echo it back. Retries within `BUDGET`, because a
/// listener that has just started may not have polled its datagram dispatcher
/// yet; every attempt after the handler is installed routes to the same
/// destination, so a retry cannot change *which* destination served it.
async fn relay_udp(addr: SocketAddr, destination: &UdpDestination, token: [u8; TOKEN_LEN]) {
    let client = UdpSocket::bind("127.0.0.1:0")
        .await
        .expect("an ephemeral loopback port must be available");
    let mut expected = [0u8; TOKEN_LEN + 2];
    expected[..2].copy_from_slice(b"E:");
    expected[2..].copy_from_slice(&token);
    let deadline = Instant::now() + BUDGET;
    loop {
        client
            .send_to(&token, addr)
            .await
            .unwrap_or_else(|e| panic!("sending a datagram to {addr} failed: {e}"));
        let mut buf = [0u8; 2048];
        match tokio::time::timeout(Duration::from_millis(200), client.recv_from(&mut buf)).await {
            Ok(Ok((len, _))) if len == expected.len() && buf[..len] == expected => break,
            _ if Instant::now() < deadline => continue,
            _ => panic!("no reply to a datagram through {addr} within {BUDGET:?}"),
        }
    }
    assert!(
        destination.saw(&token),
        "the datagram through {addr} got a reply, but that destination never read the token: \
         the relay reached some other destination"
    );
}

/// Wait, bounded, for a listener's socket to be gone: a retired listener
/// refuses the connect.
async fn await_refused(addr: &str, phase: &str) {
    let deadline = Instant::now() + BUDGET;
    loop {
        if TcpStream::connect(addr).await.is_err() {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "{phase}: {addr} still accepted a connection within {BUDGET:?}, so the listener is \
             still serving"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

/// Wait, bounded, for a listener to accept a connection.
async fn await_accepted(addr: &str, phase: &str) {
    let deadline = Instant::now() + BUDGET;
    loop {
        if TcpStream::connect(addr).await.is_ok() {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "{phase}: {addr} refused a connection for {BUDGET:?}, so the listener is not serving"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

// -- the runtime ----------------------------------------------------------------------

/// Spawn the session and retention actors, leaving them in the test's task set.
fn spawn_process_actors(tasks: &mut Tasks) -> (SessionSpawner, RetentionActorSender) {
    let (session_spawner, mut session_rx) = SessionSpawner::channel();
    tasks.spawn(async move {
        let mut sessions = JoinSet::new();
        loop {
            tokio::select! {
                Some(fut) = session_rx.recv() => { sessions.spawn(fut); }
                Some(res) = sessions.join_next() => {
                    let _ = res.expect("session task panicked");
                }
                else => break,
            }
        }
    });
    let (retention_actor, retention) = RetentionActor::new();
    tasks.spawn(async move {
        let _exit = retention_actor.run().await;
    });
    (session_spawner, retention)
}

/// The runtime every listener commits into, reading the connector
/// configuration the test's own cell holds. The connector drivers get a task
/// set of their own, so no later abort of a listener set can reach them.
fn runtime(tasks: &mut Tasks) -> Runtime {
    let (session_spawner, retention) = spawn_process_actors(tasks);
    let mut connector_drivers: ServerTasks = JoinSet::new();
    let connector_config = connector_config_cell(ConnectorConfig::default()).0;
    let udp_connector = Arc::new(UdpConnector::new(connector_config.clone()));
    let connector_table = Arc::new(build_concrete_stream_connector_table(
        connector_config,
        ConnectorResetSignal(Notify::new()),
        &mut connector_drivers,
        &udp_connector,
    ));
    tasks.spawn(async move {
        while let Some(result) = connector_drivers.join_next().await {
            result
                .expect("connector driver panicked")
                .expect("connector driver failed");
        }
    });
    let stream = StreamRuntime {
        session_table: None,
        pool: Swap::new(StreamConnPool::empty()),
        connector_table,
        replay_validator: Arc::new(ReplayValidator::new(
            VALIDATOR_TIME_FRAME,
            VALIDATOR_CAPACITY,
        )),
        session_spawner: session_spawner.clone(),
        retention: retention.clone(),
    };
    let udp = UdpRuntime {
        session_table: None,
        time_validator: Arc::new(TimeValidator::new(
            VALIDATOR_TIME_FRAME + VALIDATOR_UDP_HDR_TTL,
        )),
        connector: udp_connector,
        session_spawner: session_spawner.clone(),
        retention,
    };
    Runtime {
        stream,
        udp,
        session_spawner,
    }
}

// -- configuration --------------------------------------------------------------------

/// One generation's access-server config, built from the destination ports it
/// names. Every listener binds an ephemeral loopback port. The tcp listener is
/// always present; the udp and http listeners are added per generation.
fn generation(tcp_dest: u16, udp_dest: Option<u16>, http: bool) -> AccessServerConfig {
    let mut value = serde_json::json!({
        "tcp_server": [{
            "listen_addr": "127.0.0.1:0",
            "destination": format!("tcp://127.0.0.1:{tcp_dest}"),
            "conn_selector": { "chains": [] },
        }],
    });
    if let Some(udp_dest) = udp_dest {
        value["udp_server"] = serde_json::json!([{
            "listen_addr": "127.0.0.1:0",
            "destination": format!("127.0.0.1:{udp_dest}"),
            "conn_selector": { "chains": [] },
        }]);
    }
    if http {
        value["http_server"] = serde_json::json!([{
            "listen_addr": "127.0.0.1:0",
            "route_table": [{ "matcher": {}, "action": "direct" }],
        }]);
    }
    serde_json::from_value(value).expect("the scripted access-server config must deserialize")
}

// -- driving the production prepare/commit entry points -------------------------------

async fn prepare(
    loader: &AccessServerLoader,
    config: AccessServerConfig,
    runtime: &Runtime,
    phase: &str,
) -> access_server::PreparedAccessServer {
    let snapshot = loader.snapshot();
    let empty: HashMap<Arc<str>, HopConfig> = HashMap::new();
    access_server::prepare(
        config,
        &snapshot,
        CancellationToken::new(),
        runtime.clone(),
        &empty,
        &empty,
    )
    .await
    .unwrap_or_else(|e| panic!("{phase}: preparation must succeed, but it failed: {e}"))
}

/// Kill exactly the tasks in `tasks` and wait for them to be reaped, so their
/// handler receivers are certainly dropped — the state a listener that died on
/// an I/O error leaves behind.
async fn kill(tasks: &mut ServerTasks) {
    tasks.abort_all();
    while tasks.join_next().await.is_some() {}
}

// -- the pin --------------------------------------------------------------------------

/// A commit whose first kind loses a listener still commits the kinds that
/// follow it, and still retires what the new configuration removed from them.
///
/// Generation 3 is the tested commit: the tcp listener is aborted between
/// prepare and commit, so the tcp kind fails, while the udp kind gains a
/// listener and the http kind loses one. The udp listener must spawn and serve
/// the destination generation 3 named, and the removed http listener must stop
/// serving. Generation 4 applies the rest.
#[tokio::test(flavor = "multi_thread")]
async fn a_failed_kind_does_not_suppress_the_kinds_that_follow_it() {
    let events = capture_events();

    let mut tasks: Tasks = JoinSet::new();
    let runtime = runtime(&mut tasks);

    let tcp_old = TcpDestination::bind().await;
    let tcp_new = TcpDestination::bind().await;
    let udp_new = UdpDestination::bind().await;

    let mut loader = AccessServerLoader::new();

    // The tcp listener's task gets a set of its own, which is what lets the
    // test kill exactly that listener without touching the other kinds.
    let mut dying_tasks: ServerTasks = JoinSet::new();
    // Every other listener task.
    let mut live_tasks: ServerTasks = JoinSet::new();

    // -- generation 1: one tcp listener ----------------------------------------------
    let prepared = prepare(
        &loader,
        generation(tcp_old.port, None, false),
        &runtime,
        "g1",
    )
    .await;
    loader
        .commit(&mut dying_tasks, prepared)
        .expect("generation 1 commits into an empty loader, so nothing can fail");
    assert_eq!(
        dying_tasks.len(),
        1,
        "generation 1 spawns exactly one listener task, so the later abort kills exactly that \
         listener"
    );
    let tcp_addr = await_new_listen_addr(&events, 1, "g1").await;
    relay_tcp(&tcp_addr, &tcp_old, token(1, 0)).await;

    // -- generation 2: add the http listener that generation 3 removes ---------------
    let prepared = prepare(
        &loader,
        generation(tcp_old.port, None, true),
        &runtime,
        "g2",
    )
    .await;
    loader
        .commit(&mut live_tasks, prepared)
        .expect("generation 2 must commit cleanly");
    let http_addr = await_new_listen_addr(&events, 2, "g2").await;
    await_accepted(&http_addr, "g2: the http listener").await;

    // -- generation 3: prepare, kill the tcp listener, commit ------------------------
    let prepared = prepare(
        &loader,
        generation(tcp_new.port, Some(udp_new.addr.port()), false),
        &runtime,
        "g3",
    )
    .await;
    // Preparation bound every socket it needs and touched nothing live; the
    // tcp listener is alive at this instant, which is what makes the commit
    // build a handler replacement for it rather than a fresh listener.
    assert_eq!(
        listen_addrs(&events).len(),
        2,
        "preparation must not start a listener: it binds sockets and spawns no task"
    );
    kill(&mut dying_tasks).await;

    let error = loader.commit(&mut live_tasks, prepared).expect_err(
        "a commit whose tcp listener died between preparation and commit must report the \
             lost handler update rather than swallow it",
    );
    assert!(
        error.to_string().contains("listener died"),
        "the reported failure must name what happened; got: {error}"
    );
    assert!(
        error.to_string().contains("tcp_server"),
        "the reported failure must name the kind that lost the update; got: {error}"
    );

    // The udp kind follows the failed tcp kind, so its newly configured
    // listener is spawned: it reports its address and serves the destination
    // generation 3 named, not the one any earlier generation named.
    let udp_addr = await_new_listen_addr(&events, 3, "g3: the udp kind after a failed tcp kind")
        .await
        .parse::<SocketAddr>()
        .expect("a listener reports a socket address");
    relay_udp(udp_addr, &udp_new, token(3, 0)).await;

    // The http kind follows the failed tcp kind, so its removal is committed:
    // the listener the new configuration dropped is retired.
    await_refused(
        &http_addr,
        "g3: the http listener the configuration removed",
    )
    .await;

    // The tcp listener the failed kind could not update is dead: its socket is
    // closed, and the next commit re-spawns it.
    await_refused(&tcp_addr, "g3: the tcp listener that died").await;

    // -- generation 4: the same configuration again, committed ----------------------
    let prepared = prepare(
        &loader,
        generation(tcp_new.port, Some(udp_new.addr.port()), false),
        &runtime,
        "g4",
    )
    .await;
    loader
        .commit(&mut live_tasks, prepared)
        .expect("generation 4 is generation 3's configuration committed with no listener dying, so it must commit cleanly");
    let tcp_addr = await_new_listen_addr(&events, 4, "g4: the re-spawned tcp listener").await;
    relay_tcp(&tcp_addr, &tcp_new, token(4, 0)).await;

    // The destinations the failed commit named never served a session: the tcp
    // kind's old listener was dead, and no listener ever routed to the old
    // tcp destination after the kill.
    assert_eq!(
        (tcp_old.accepts(), tcp_new.accepts()),
        (1, 1),
        "each tcp destination must have served exactly the session its generation named"
    );

    tasks.shutdown().await;
}
