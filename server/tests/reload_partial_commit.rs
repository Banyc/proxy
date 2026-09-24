//! Pins what a reload commit installs when one listener dies between
//! preparation and commit, and what it leaves serving.
//!
//! `commit_reload` swaps the stream pool and the shared connector
//! configuration first and then commits all three listener loaders, in order
//! (`access_server`, `proxy_server`, `reverse_tunnel`), collecting each
//! loader's failure instead of short-circuiting. A listener that died since
//! preparation fails its own op, and the loader keeps going: the ops of one
//! preparation are independent, so the later ops of that loader still apply,
//! every other loader is still committed, and every loader's retirement still
//! runs. The design accepts the resulting state deliberately —
//! `common/src/loading.rs`'s doc on `Loader::commit` says the caller must
//! surface a lost handler update even though "the global state may already
//! have been swapped", and the serve loop reports rather than rolls back and
//! never retries — so the point of this test is to fix *which* pieces of the
//! new configuration are live in that state and which are not:
//!
//! - the globals (stream pool, connector configuration) are the new
//!   generation's;
//! - a listener whose handler replacement can be delivered adopts it even when
//!   an earlier listener of the same loader died, and relays to the destination
//!   the new generation named;
//! - a listener the new configuration added is spawned even when the failed op
//!   precedes it in its loader's preparation order;
//! - a listener the new configuration removed is retired, because the loader
//!   that names it is still committed and retirement runs even inside a loader
//!   whose own commit failed;
//! - the listener that died is gone, and the next commit of the same
//!   configuration re-spawns it.
//!
//! Reaching that state needs a listener to die in the window between
//! preparation and commit, which no configuration can express: a listener
//! only dies on its own `serve` failure. The test therefore composes the two
//! production entry points (`prepare_reload` and `commit_reload`) directly
//! instead of driving `serve`, and kills the listener by aborting the task
//! the previous commit spawned for it — the same closed
//! `ReplaceConnHandlerRx` a listener that died on an I/O error leaves behind.
//! Every listener still binds an ephemeral port on a loopback address, and
//! its address is read back from the listener's own `Listening` event, so no
//! port is ever raced.
//!
//! The listener tasks live in JoinSets the test owns, because it drives the
//! loaders directly. The generation that must lose a listener gets its own
//! JoinSet, which is exactly the state a listener that died on its own leaves
//! the server's task set in: one task that has returned.

use std::{
    collections::HashMap,
    io,
    net::Ipv4Addr,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};

use ae::anti_replay::{ReplayValidator, TimeValidator};
use common::{
    anti_replay::{VALIDATOR_CAPACITY, VALIDATOR_TIME_FRAME, VALIDATOR_UDP_HDR_TTL},
    connect::{
        ConnectorConfig, ConnectorConfigReader, ConnectorConfigUpdater, ConnectorResetSignal,
        connector_config_cell,
    },
    error::{AnyError, AnyResult},
    lifecycle::retention::{RetentionActor, RetentionActorSender},
    notify::Notify,
    proxy_runtime::{
        connect::udp::UdpConnector,
        context::{Runtime, StreamRuntime, UdpRuntime},
    },
    session::SessionSpawner,
    stream_runtime::pool::StreamConnPool,
};
use protocol::{
    access_server::AccessServerLoader, proxy_server::ProxyServerLoader,
    reverse_tunnel::ReverseTunnelLoader,
    stream_proto::connect::build_concrete_stream_connector_table,
};
use server::{
    ServerConfig, ServerLoader,
    config::ReadConfig,
    reload::{PreparedReload, commit_reload, prepare_reload},
};
use swap::Swap;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    task::JoinSet,
};
use tokio_util::sync::{CancellationToken, DropGuard};

/// Sessions relay a fixed-size token and the destination echoes it, so a
/// relay is byte-exact and which destination served a session is read off the
/// token's identity, never off timing.
const TOKEN_LEN: usize = 16;

/// The example header key, used by the reverse-tunnel responder.
const HEADER_KEY: &str = "cHJveHktZXhhbXBsZS1rZXk";

/// The loader key of the listener that dies, and the keys of the two
/// listeners that must be distinct entries of the same loader. All three bind
/// an ephemeral port; only the address string is a key, so `[::1]:0` inside
/// one `tcp_server` array is a different listener from `127.0.0.1:0`.
const DYING_KEY: &str = "127.0.0.1:0";
const SURVIVING_KEY: &str = "[::1]:0";
const ADDED_KEY: &str = "localhost:0";

/// Every await that waits for an effect a commit triggers (a listener
/// starting, a socket closing) is bounded by this. A budget that expires is
/// reported with the phase that was waiting, so a stall names itself.
const BUDGET: Duration = Duration::from_secs(10);

/// The message the serve loop logs when it starts a listener, carrying the
/// address it bound in an `addr` field.
const LISTEN_MESSAGE: &str = "Listening";

/// The test's tasks: the process actors live here, so returning from the test
/// aborts them.
type Tasks = JoinSet<()>;

/// The server-task set the production loop passes to `commit_reload`; new
/// listener tasks land here.
type ServerTasks = JoinSet<AnyResult>;

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

fn rendered(buf: &Arc<Mutex<Vec<u8>>>) -> String {
    String::from_utf8(buf.lock().unwrap().clone()).expect("every event renders as UTF-8")
}

/// Every address a listener has reported through its `Listening` event, in
/// the order the events landed. The listener that bound the socket is the one
/// that logs it, so the address is what it actually bound.
fn listen_addrs(buf: &Arc<Mutex<Vec<u8>>>) -> Vec<String> {
    rendered(buf)
        .lines()
        .filter(|line| line.contains(LISTEN_MESSAGE))
        .filter_map(|line| line.split("addr=").nth(1))
        .filter_map(|rest| rest.split_whitespace().next())
        .map(|addr| addr.trim_end_matches('"').to_owned())
        .filter(|addr| !addr.is_empty())
        .collect()
}

/// Wait, bounded, until `want` listeners have reported their address. A
/// listener logs on its first poll, which the commit schedules, so the count
/// is a commit effect and not a timer.
async fn await_listen_addrs(buf: &Arc<Mutex<Vec<u8>>>, want: usize, phase: &str) -> Vec<String> {
    let deadline = Instant::now() + BUDGET;
    loop {
        let addrs = listen_addrs(buf);
        if addrs.len() >= want {
            return addrs;
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

/// The address a listener bound, out of the addresses the log holds, keyed by
/// the family the loader key names: the survivor binds `[::1]` and the
/// responder `127.0.0.1`, so the two are told apart without an assumption
/// about which task polled first.
fn pick_addr(addrs: &[String], bracketed: bool) -> String {
    addrs
        .iter()
        .find(|addr| addr.starts_with('[') == bracketed)
        .unwrap_or_else(|| {
            panic!(
                "no {} address among the listeners that reported one: {addrs:?}",
                if bracketed { "bracketed" } else { "plain" }
            )
        })
        .clone()
}

// -- destinations ---------------------------------------------------------------------

/// A test-owned TCP origin: it counts accepted connections, records every
/// token it reads, and echoes each one back prefixed with `E:`. A distinct
/// port per generation is what makes "which destination received the bytes"
/// an exact question.
struct Destination {
    port: u16,
    accepts: Arc<AtomicUsize>,
    received: Arc<Mutex<Vec<[u8; TOKEN_LEN]>>>,
    _tasks: JoinSet<()>,
}

impl Destination {
    async fn bind() -> Self {
        // A heavily port-binding suite can exhaust the loopback allocator;
        // retry the bind, which is a host condition and not a failure here.
        let mut listener = None;
        for _ in 0..16 {
            match TcpListener::bind("127.0.0.1:0").await {
                Ok(bound) => {
                    listener = Some(bound);
                    break;
                }
                Err(_) => tokio::time::sleep(Duration::from_millis(20)).await,
            }
        }
        let listener = listener.expect("an ephemeral loopback port must be available");
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

/// The whole routing ledger of a phase: which destination has accepted which
/// session, and which tokens each has read. Asserting the ledger after every
/// commit is what makes "which traffic follows which configuration" exact.
fn ledger(destinations: &[(&str, &Destination)]) -> HashMap<String, (usize, Vec<[u8; TOKEN_LEN]>)> {
    destinations
        .iter()
        .map(|(name, destination)| {
            (
                (*name).to_owned(),
                (
                    destination.accepts(),
                    destination.received.lock().unwrap().clone(),
                ),
            )
        })
        .collect()
}

// -- clients --------------------------------------------------------------------------

/// Open a session through a listener at `addr`, send `token`, and require its
/// echo. Success means the bytes reached a destination and came back, so the
/// caller attributes the session by which destination recorded the token.
async fn open_session(addr: &str, token: [u8; TOKEN_LEN]) -> io::Result<TcpStream> {
    let mut stream = tokio::time::timeout(BUDGET, TcpStream::connect(addr))
        .await
        .map_err(|_| io::Error::other("connect timed out"))??;
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
    .map_err(|_| io::Error::other("relay timed out"))??;
    Ok(stream)
}

/// Send `token` through `addr` and require `destination` to record it.
async fn relay_reaches(addr: &str, destination: &Destination, token: [u8; TOKEN_LEN]) {
    open_session(addr, token)
        .await
        .unwrap_or_else(|e| panic!("a session through {addr} must relay: {e}"));
    assert!(
        destination.saw(&token),
        "the session through {addr} relayed, but that destination never read the token: the \
         relay reached some other destination"
    );
}

/// Wait, bounded, for a listener's socket to be gone: a retired listener
/// refuses the connect. The window exists because the despawn is asynchronous,
/// not because the assertion is allowed to be one.
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

/// Wait, bounded, for a listener to accept a connection. The responder needs
/// no handshake for this: its socket being open is the observable.
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

/// A [`ReadConfig`] serving one config text, parsed per read.
struct ConfigText(String);
impl ReadConfig for ConfigText {
    type Config = ServerConfig;

    async fn read_config(&self) -> Result<ServerConfig, AnyError> {
        Ok(toml::from_str(&self.0)?)
    }
}

/// Spawn the session and retention actors, leaving them in the test's task
/// set.
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
fn runtime(tasks: &mut Tasks, connector_config: ConnectorConfigReader) -> Runtime {
    let (session_spawner, retention) = spawn_process_actors(tasks);
    let mut connector_drivers: ServerTasks = JoinSet::new();
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

/// One access-server TCP listener: `listen_addr` is its loader key, so two
/// entries of the same generation need distinct address strings.
fn tcp_listener(listen_addr: &str, destination_port: u16) -> String {
    format!(
        "[[access_server.tcp_server]]\n\
         listen_addr = \"{listen_addr}\"\n\
         destination = \"tcp://127.0.0.1:{destination_port}\"\n\
         conn_selector = \"default\"\n\n"
    )
}

/// The generation's access-server TCP listeners, in configuration order —
/// which is the order `Loader::commit` applies them in — plus optionally a
/// reverse-tunnel responder, and optionally an outbound bind address that
/// makes this generation's connector configuration distinguishable from the
/// previous one's.
fn generation(note: &str, listeners: &[(&str, u16)], responder: bool, outbound_v4: bool) -> String {
    let mut out = format!(
        "# {note}\n\
         [access_server.stream.conn_selector]\n\
         \"default\" = {{ chains = [] }}\n\n"
    );
    if outbound_v4 {
        out.push_str("[connector.bind]\nv4 = \"127.0.0.1\"\n\n");
    }
    for (listen_addr, destination_port) in listeners {
        out.push_str(&tcp_listener(listen_addr, *destination_port));
    }
    if responder {
        out.push_str(&format!(
            "[[reverse_tunnel.responder]]\n\
             listen_addr = \"tcp://127.0.0.1:0\"\n\
             header_key = \"{HEADER_KEY}\"\n"
        ));
    }
    out
}

// -- driving the production reload entry points ---------------------------------------

async fn prepare(
    reader: &Arc<ConfigText>,
    loader: &ServerLoader,
    runtime: &Runtime,
    phase: &str,
) -> PreparedReload {
    prepare_reload(
        Arc::clone(reader),
        loader.snapshot(),
        CancellationToken::new(),
        runtime.clone(),
    )
    .await
    .unwrap_or_else(|e| panic!("{phase}: preparation must succeed, but it failed: {e}"))
}

fn commit(
    runtime: &Runtime,
    loader: &mut ServerLoader,
    updater: &ConnectorConfigUpdater,
    tasks: &mut ServerTasks,
    prepared: PreparedReload,
) -> (DropGuard, Option<AnyError>) {
    commit_reload(tasks, loader, prepared, runtime, updater)
}

// -- the pin --------------------------------------------------------------------------

/// A commit that loses a listener installs the new generation's globals,
/// reports the lost update, and still applies every other op of the failed
/// loader and every other loader — so the listener after the failed op adopts
/// the new generation's destination, one the configuration added after it
/// starts, and one the new configuration removed is retired.
///
/// Four commits of a scripted configuration:
///
/// 1. one listener (`DYING_KEY`), whose task the test owns;
/// 2. a generation that keeps it and adds a surviving listener
///    (`SURVIVING_KEY`) plus a reverse-tunnel responder;
/// 3. a generation that re-points both access listeners, adds a third
///    (`ADDED_KEY`), removes the responder, and changes the connector
///    configuration's bind address — prepared while the dying listener is
///    alive, committed after it is killed, so the access-server commit fails at
///    its first step while the other loaders still commit;
/// 4. the same configuration again, which must apply everything the failed
///    commit left behind.
#[tokio::test(flavor = "multi_thread")]
async fn a_commit_that_loses_a_listener_installs_the_globals_and_commits_every_other_listener() {
    let events = capture_events();

    let mut tasks: Tasks = JoinSet::new();
    let (connector_reader, connector_updater) = connector_config_cell(ConnectorConfig::default());
    let runtime = runtime(&mut tasks, connector_reader.clone());

    let dying_old = Destination::bind().await;
    let dying_new = Destination::bind().await;
    let surviving_old = Destination::bind().await;
    let surviving_new = Destination::bind().await;
    let added = Destination::bind().await;

    let mut loader = ServerLoader {
        access_server: AccessServerLoader::new(),
        proxy_server: ProxyServerLoader::new(),
        reverse_tunnel: ReverseTunnelLoader::new(),
    };

    // The dying listener's task gets a set of its own, which is what lets the
    // test kill exactly that listener and what a listener that died on its
    // own leaves behind: a task set holding one task that has returned.
    let mut dying_tasks: ServerTasks = JoinSet::new();
    // Every other listener task, and the responder's.
    let mut live_tasks: ServerTasks = JoinSet::new();

    // -- generation 1: one listener --------------------------------------------------
    let reader = Arc::new(ConfigText(generation(
        "g1",
        &[(DYING_KEY, dying_old.port)],
        false,
        false,
    )));
    let mut _generation_guard;
    let prepared = prepare(&reader, &loader, &runtime, "g1").await;
    let (guard, error) = commit(
        &runtime,
        &mut loader,
        &connector_updater,
        &mut dying_tasks,
        prepared,
    );
    _generation_guard = guard;
    assert!(
        error.is_none(),
        "generation 1 commits into an empty loader, so nothing can fail: {error:?}"
    );
    assert_eq!(
        dying_tasks.len(),
        1,
        "generation 1 spawns exactly one listener task, so the later abort kills exactly that \
         listener"
    );
    let addrs = await_listen_addrs(&events, 1, "g1").await;
    assert_eq!(addrs.len(), 1, "generation 1 starts exactly one listener");
    let dying_addr = addrs[0].clone();
    relay_reaches(&dying_addr, &dying_old, token(1, 0)).await;

    // -- generation 2: keep it, add the survivor and the responder -------------------
    let reader = Arc::new(ConfigText(generation(
        "g2",
        &[
            (DYING_KEY, dying_old.port),
            (SURVIVING_KEY, surviving_old.port),
        ],
        true,
        false,
    )));
    let prepared = prepare(&reader, &loader, &runtime, "g2").await;
    let (next_guard, error) = commit(
        &runtime,
        &mut loader,
        &connector_updater,
        &mut live_tasks,
        prepared,
    );
    _generation_guard = next_guard;
    assert!(
        error.is_none(),
        "generation 2 must commit cleanly: {error:?}"
    );
    let addrs = await_listen_addrs(&events, 3, "g2").await;
    assert_eq!(
        addrs.len(),
        3,
        "generation 2 adds exactly two listeners: the survivor and the responder"
    );
    // The survivor's loader key is `[::1]:0`, so the address it binds and
    // logs is the bracketed one; the responder binds `127.0.0.1`. Both are
    // ephemeral, so the discriminator is the address family, never a port.
    let surviving_addr = pick_addr(&addrs[1..], true);
    let responder_addr = pick_addr(&addrs[1..], false);
    relay_reaches(&surviving_addr, &surviving_old, token(2, 0)).await;
    await_accepted(&responder_addr, "g2: the responder").await;

    // -- generation 3: prepare, kill the listener, commit ---------------------------
    let reader = Arc::new(ConfigText(generation(
        "g3",
        &[
            (DYING_KEY, dying_new.port),
            (SURVIVING_KEY, surviving_new.port),
            (ADDED_KEY, added.port),
        ],
        false,
        true,
    )));
    let prepared = prepare(&reader, &loader, &runtime, "g3").await;
    // Preparation bound every socket it needs and touched nothing live; the
    // dying listener is alive at this instant, which is what makes the commit
    // build a handler replacement for it rather than a fresh listener.
    let listeners_before = listen_addrs(&events).len();
    assert_eq!(
        listeners_before, 3,
        "preparation must not start a listener: it binds sockets and spawns no task"
    );

    // Kill it, and reap it, so its handler receiver is certainly dropped —
    // the state a listener that died on an I/O error leaves behind.
    dying_tasks.abort_all();
    while dying_tasks.join_next().await.is_some() {}

    let pool_before = runtime.stream.pool.inner();
    let (next_guard, commit_error) = commit(
        &runtime,
        &mut loader,
        &connector_updater,
        &mut live_tasks,
        prepared,
    );
    let pool_after = runtime.stream.pool.inner();
    _generation_guard = next_guard;

    let error = commit_error.expect(
        "a commit whose listener died between preparation and commit must report the lost \
         handler update rather than swallow it",
    );
    assert!(
        error.to_string().contains("listener died"),
        "the reported failure must name what happened; got: {error}"
    );

    // The globals are the new generation's: they are swapped before the
    // listener commit, and nothing rolls them back.
    let pool_replaced = !Arc::ptr_eq(&*pool_before, &*pool_after);
    drop(pool_before);
    drop(pool_after);
    assert!(
        pool_replaced,
        "the failed commit must still have installed the new generation's stream pool: the \
         global swap precedes the listener commit and is not rolled back"
    );
    assert_eq!(
        connector_reader.current().bind.v4,
        Some(Ipv4Addr::LOCALHOST),
        "the failed commit must still have installed the new generation's connector \
         configuration: the shared cell is written before the listener commit"
    );

    // What the failed loader would have applied after the failed op is
    // applied: the ops of one preparation are independent, so the failed
    // replacement of the listener that died neither forfeits the replacement
    // of the survivor that follows it nor the spawn of the listener the
    // configuration added after both. The survivor comes first, because it is
    // the content the failed op's own loader still had to deliver: it relays
    // to the destination this generation named, and never to the one the
    // previous generation named — the token, and the two separate
    // destinations, are what say so.
    relay_reaches(&surviving_addr, &surviving_new, token(3, 0)).await;

    // Exactly one listener starts here, so its position in the log is
    // unambiguous, and its server was bound during preparation and carries its
    // destination itself, so it relays as soon as it is spawned.
    let addrs = await_listen_addrs(&events, listeners_before + 1, "g3: the added listener").await;
    assert_eq!(
        addrs.len(),
        listeners_before + 1,
        "the failed commit must still have started the listener the configuration added"
    );
    let added_addr = addrs[listeners_before].clone();
    relay_reaches(&added_addr, &added, token(3, 1)).await;

    // The dying listener is gone: its socket is closed.
    await_refused(&dying_addr, "g3: the listener that died").await;

    // The listener the new configuration removed is retired: the responder's
    // loader is committed even though access_server's failed, and retirement
    // runs even inside a loader whose own commit failed.
    await_refused(
        &responder_addr,
        "g3: the responder the configuration removed",
    )
    .await;

    // Exactly the destinations this generation named were reached: the
    // survivor's replacement and the added listener's spawn took their
    // sessions, and the listener that died took none.
    assert!(
        !surviving_old.saw(&token(3, 0)),
        "the surviving listener stayed on the previous generation's handler instead of adopting \
         the one the failed commit could deliver"
    );
    assert_eq!(
        (
            surviving_new.accepts(),
            dying_new.accepts(),
            added.accepts()
        ),
        (1, 0, 1),
        "a destination this generation named accepted a session other than the one routed to \
         it, or the listener that died relayed to one of them"
    );

    // -- generation 4: the same configuration again, committed ----------------------
    let reader = Arc::new(ConfigText(generation(
        "g4",
        &[
            (DYING_KEY, dying_new.port),
            (SURVIVING_KEY, surviving_new.port),
            (ADDED_KEY, added.port),
        ],
        false,
        true,
    )));
    let prepared = prepare(&reader, &loader, &runtime, "g4").await;
    let (next_guard, error) = commit(
        &runtime,
        &mut loader,
        &connector_updater,
        &mut live_tasks,
        prepared,
    );
    _generation_guard = next_guard;
    assert!(
        error.is_none(),
        "generation 4 is generation 3's configuration committed with no listener dying, so it \
         must commit cleanly: {error:?}"
    );

    let addrs = await_listen_addrs(&events, listeners_before + 2, "g4").await;
    assert_eq!(
        addrs.len(),
        listeners_before + 2,
        "generation 4 starts exactly one listener: it re-spawns the one that died, while the \
         listener the failed commit added is already serving"
    );

    // The one address generation 4 starts is the re-spawned listener, and it
    // relays to the destination this configuration names for its key — never to
    // the one the listener that died was serving. The address the failed commit
    // started still serves the destination it was bound with.
    let respawned_addr = addrs[listeners_before + 1].clone();
    relay_reaches(&respawned_addr, &dying_new, token(4, 0)).await;
    relay_reaches(&added_addr, &added, token(4, 1)).await;

    // The survivor's handler is re-applied to the destination it adopted in
    // generation 3, and it still relays there.
    relay_reaches(&surviving_addr, &surviving_new, token(4, 2)).await;

    // ...and the listener the previous configuration had asked to remove is
    // still retired, not resurrected by the commit that applies the rest.
    await_refused(
        &responder_addr,
        "g4: the responder the configuration removed",
    )
    .await;

    // The ledger, exactly: every destination accepted the sessions that were
    // routed to it and no others, so no session was diverted to a destination
    // its generation did not name, and no listener kept a handler its
    // generation did not name.
    let expected: HashMap<String, (usize, Vec<[u8; TOKEN_LEN]>)> = HashMap::from([
        ("dying_old".to_owned(), (1, vec![token(1, 0)])),
        ("dying_new".to_owned(), (1, vec![token(4, 0)])),
        ("surviving_old".to_owned(), (1, vec![token(2, 0)])),
        (
            "surviving_new".to_owned(),
            (2, vec![token(3, 0), token(4, 2)]),
        ),
        ("added".to_owned(), (2, vec![token(3, 1), token(4, 1)])),
    ]);
    assert_eq!(
        ledger(&[
            ("dying_old", &dying_old),
            ("dying_new", &dying_new),
            ("surviving_old", &surviving_old),
            ("surviving_new", &surviving_new),
            ("added", &added),
        ]),
        expected,
        "the routing ledger must match exactly: every session reached the destination its \
         generation named, and no destination the configuration no longer names received one"
    );

    drop(_generation_guard);
    tasks.shutdown().await;
}
