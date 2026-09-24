//! Pins the reload controller's per-generation listener lifecycle through the
//! one effect a committed generation has outside the process: the route-chain
//! RTT probes it owns.
//!
//! `prepare_reload` takes a fresh [`tokio_util::sync::CancellationToken`] per
//! generation and disarms its `CancelOnDrop` guard on success, so a prepared
//! generation keeps a live token. `commit_reload` returns that token's
//! `DropGuard`, `serve` holds it in `_cancellation_guard`, and the assignment
//! that installs the next generation's guard drops the previous one — which is
//! what retires the generation it replaces. The token reaches exactly one live
//! consumer: `GaugedRouteChain`'s probe task. A generation that is alive
//! therefore probes its hop, and a generation that has been retired stops.
//!
//! The oracle is a pair of test-owned TCP listeners bound at `127.0.0.1:0`.
//! The test is the dialed party, so it never has to learn a port the proxy
//! chose and never races one: both listeners bind an ephemeral port, so no
//! bind can be taken and none needs a retry. `probe_rtt = true` puts the
//! chains' probe tasks in the generation, and a probe task issues its first
//! round immediately on spawn, so both generations dial on a deterministic
//! trigger (commit) rather than on a timer.
//!
//! Generation 0's selector names listener A; the reloaded generation's names
//! listener B. The test drives the reload through the production serve loop's
//! config reader, so the assertion that B is dialed is an assertion about the
//! generation the loop actually committed. The only wall-clock waits are the
//! loop's own debounce (awaited as a config read, never as a duration), a
//! settle, and the superseded-generation window; none is an expected-latency
//! assertion.

use std::{
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};

use common::{
    error::AnyError,
    lifecycle::{retention::RetentionActor, suspend::SystemResumeSignal},
    notify::Notify,
};
use server::{
    ServeContext, ServerConfig,
    config::{ConfigChangeSignal, ReadConfig},
    serve,
};
use tokio::net::TcpListener;

/// How many independent probed chains each generation's selector names. Each
/// chain gets its own probe task, so this is also the number of concurrent
/// probe streams a live generation runs — which is what makes the
/// superseded-generation window below a strong detector rather than a hopeful
/// one. The count is asserted, not assumed: the test requires a full burst of
/// [`PROBE_CHAINS`] connections from the generation under observation.
const PROBE_CHAINS: usize = 64;

/// The example header key, shared by every hop in these configs.
const HEADER_KEY: &str = "cHJveHktZXhhbXBsZS1rZXk";

/// A listener the test owns. It counts every accepted connection and drops it
/// at once, so a probe round fails immediately instead of holding the probe
/// task for `STREAM_IO_TIMEOUT` and spacing the rounds out.
struct ProbeTarget {
    port: u16,
    accepts: Arc<AtomicUsize>,
    /// Object-owned: dropping the target aborts the accept loop instead of
    /// leaving it detached.
    _accept_loop: tokio::task::JoinSet<()>,
}

impl ProbeTarget {
    async fn bind() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("binding an ephemeral loopback listener must succeed");
        let port = listener
            .local_addr()
            .expect("a bound listener has a local address")
            .port();
        let accepts = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&accepts);
        let mut accept_loop = tokio::task::JoinSet::new();
        accept_loop.spawn(async move {
            while let Ok((stream, _peer)) = listener.accept().await {
                counter.fetch_add(1, Ordering::SeqCst);
                // Drop immediately: the probe's handshake write/read then
                // fails, so each probe round is short and the task returns to
                // its sleep instead of blocking on a response.
                drop(stream);
            }
        });
        Self {
            port,
            accepts,
            _accept_loop: accept_loop,
        }
    }

    fn accepts(&self) -> usize {
        self.accepts.load(Ordering::SeqCst)
    }

    /// Wait, bounded by `budget`, until `want` connections have been accepted.
    /// The budget is a liveness bound on a deterministic trigger, not an
    /// expected latency: the generation's probes dial as soon as they are
    /// spawned at commit.
    async fn await_accepts(&self, want: usize, budget: Duration, generation: &str) {
        let deadline = Instant::now() + budget;
        loop {
            let seen = self.accepts();
            if seen >= want {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "{generation} probed its hop {seen} time(s) within {budget:?}; \
                 expected at least {want}, one per chain. A committed generation's \
                 probe tasks dial as soon as they are spawned, so a generation that \
                 never reaches {want} has had its listener-generation token cancelled \
                 (or never armed) before its probes could run."
            );
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }
}

/// A config whose access-server stream selector names [`PROBE_CHAINS`] chains,
/// all pointing at a hop on `probe_port`, with RTT probing on so each chain's
/// probe task belongs to the generation.
fn probing_config(probe_port: u16) -> String {
    let chains = std::iter::repeat_n("{ weight = 1, chain = [\"hop\"] }", PROBE_CHAINS)
        .collect::<Vec<_>>()
        .join(", ");
    format!(
        r#"
[stream.upstream]
"hop" = {{ address = "tcp://127.0.0.1:{probe_port}", header_key = "{HEADER_KEY}" }}

[access_server.stream.conn_selector]
"default" = {{ chains = [{chains}], probe_rtt = true }}

[[access_server.tcp_server]]
listen_addr = "127.0.0.1:0"
destination = "tcp://127.0.0.1:9"
conn_selector = "default"
"#
    )
}

/// A [`ReadConfig`] that serves a scripted sequence of TOML configs (the last
/// one repeating) and reports each read index on a channel, so the test can
/// await a specific read instead of sleeping.
struct ScriptedReader {
    configs: std::sync::Mutex<Vec<String>>,
    reads: tokio::sync::mpsc::Sender<usize>,
    count: AtomicUsize,
}

impl ScriptedReader {
    fn new(configs: Vec<String>, reads: tokio::sync::mpsc::Sender<usize>) -> Self {
        Self {
            configs: std::sync::Mutex::new(configs),
            reads,
            count: AtomicUsize::new(0),
        }
    }
}

impl ReadConfig for ScriptedReader {
    type Config = ServerConfig;

    async fn read_config(&self) -> Result<ServerConfig, AnyError> {
        let index = self.count.fetch_add(1, Ordering::SeqCst);
        // Take the lock only to select the config; never across an await.
        let src = {
            let configs = self.configs.lock().unwrap();
            configs
                .get(index)
                .or_else(|| configs.last())
                .cloned()
                .expect("the scripted reader always has a config")
        };
        let _ = self.reads.send(index).await;
        let config: ServerConfig = toml::from_str(&src)?;
        Ok(config)
    }
}

/// Bound every await so a wedged serve loop fails instead of hanging.
async fn recv_config(rx: &mut tokio::sync::mpsc::Receiver<usize>) -> Option<usize> {
    tokio::time::timeout(std::time::Duration::from_secs(30), rx.recv())
        .await
        .expect("timed out waiting for the serve loop to read a config")
}

/// A change notification is broadcast over a `watch` generation counter and is
/// only observed by a subscriber that already exists. The serve loop
/// subscribes *after* its initial commit, so a notification sent in that
/// window is dropped. Retry the change until the serve loop's next config read
/// lands on the channel; the channel, not the delay, is the success signal.
async fn await_next_read(
    rx: &mut tokio::sync::mpsc::Receiver<usize>,
    config_changed: &ConfigChangeSignal,
    expected: usize,
) {
    for _ in 0..10 {
        config_changed.0.notify_waiters();
        match tokio::time::timeout(std::time::Duration::from_secs(2), rx.recv()).await {
            Ok(Some(index)) => {
                assert_eq!(index, expected, "unexpected config read index");
                return;
            }
            Ok(None) => panic!("the serve loop dropped its config reader"),
            Err(_) => continue,
        }
    }
    panic!("the serve loop never read config #{expected} after repeated change notifications");
}

/// A reload installs a live listener generation and retires the one it
/// replaces.
///
/// The reloaded generation must still be served by its own probe tasks: the
/// test-owned hop it names is dialed. The superseded generation must be gone:
/// the hop *it* named stops being dialed. Both directions are read from the
/// same observable (probe dials), so a runtime that never retired anything
/// would fail the second half however healthy the first looked.
#[tokio::test(flavor = "multi_thread")]
async fn a_reload_installs_a_live_generation_and_retires_the_one_it_replaces() {
    let initial_hop = ProbeTarget::bind().await;
    let reloaded_hop = ProbeTarget::bind().await;
    assert_ne!(
        initial_hop.port, reloaded_hop.port,
        "the two generations must probe distinct hops, or their dials cannot be told apart"
    );

    let (reads_tx, mut reads_rx) = tokio::sync::mpsc::channel(16);
    let reader = ScriptedReader::new(
        vec![
            probing_config(initial_hop.port),
            probing_config(reloaded_hop.port),
        ],
        reads_tx,
    );
    let config_changed = ConfigChangeSignal(Notify::new());
    let (retention_actor, retention) = RetentionActor::new();
    let context = ServeContext {
        stream_session_table: None,
        udp_session_table: None,
        config_changed: config_changed.clone(),
        system_resume: SystemResumeSignal(Notify::new()),
        retention,
    };
    let mut tasks = tokio::task::JoinSet::new();
    tasks.spawn(async move {
        let _exit = retention_actor.run().await;
    });
    tasks.spawn(async move {
        let _exit = serve(reader, context).await;
    });

    // The initial generation is read and committed; every listener binds.
    assert_eq!(
        recv_config(&mut reads_rx).await,
        Some(0),
        "serve must read the initial config"
    );

    // ...and it is alive: a committed generation's probe tasks dial their hop
    // as soon as they are spawned, so a full burst of one connection per chain
    // arrives on the hop this generation named.
    initial_hop
        .await_accepts(
            PROBE_CHAINS,
            Duration::from_secs(10),
            "the initial generation",
        )
        .await;

    // A config change drives a reload; the serve loop reads the config that
    // names the other hop. The loop's read is the event awaited here, so the
    // debounce window is never asserted on: it elapses inside the loop exactly
    // once and the resulting read is the trigger for everything below.
    await_next_read(&mut reads_rx, &config_changed, 1).await;

    // The reloaded generation is alive. This is the direction the generation
    // token governs: `prepare_reload` must hand a still-live token to the
    // commit path, and `serve` must not cancel it when it installs the guard
    // for the generation being replaced.
    reloaded_hop
        .await_accepts(
            PROBE_CHAINS,
            Duration::from_secs(10),
            "the generation installed by the reload",
        )
        .await;

    // The superseded generation must be gone. Cancellation is observed through
    // the same probe stream, so the check is a windowed one: a generation that
    // was never retired would keep dialing, and [`PROBE_CHAINS`] independent
    // probe streams each drawing a Poisson gap (mean 6s while healthy, 24s once
    // the prober backs off after repeated failures, floored at 500ms) miss a
    // window of `RETIRED_WINDOW` with probability
    // `exp(-RETIRED_WINDOW / 24s) ^ PROBE_CHAINS` ≈ 2.3e-5. The settle first
    // absorbs a dial already in flight when the guard was dropped, which
    // cannot outlive a loopback round.
    tokio::time::sleep(Duration::from_millis(300)).await;
    const RETIRED_WINDOW: Duration = Duration::from_secs(4);
    let retired_hop_dials = initial_hop.accepts();
    tokio::time::sleep(RETIRED_WINDOW).await;
    assert_eq!(
        initial_hop.accepts(),
        retired_hop_dials,
        "the superseded generation dialed its hop again during {RETIRED_WINDOW:?} \
         after the reload was committed: its generation token was not cancelled when \
         the new generation's guard replaced it"
    );

    tasks.shutdown().await;
}
