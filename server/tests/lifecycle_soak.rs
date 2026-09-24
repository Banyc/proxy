//! Liveness soak of the proxy runtime's own lifecycle.
//!
//! Each instance drives one real `proxy` process through repeated reload
//! cycles and asserts, on every cycle, the lifecycle facts a single scripted
//! sequence can miss: a defect that needs many reloads to appear (a lost
//! wakeup, a reload path that stalls, a generation that is never retired, a
//! route that is never swapped) is caught by the volume.
//!
//! One cycle drives, through the process's own config-file watcher:
//!
//! 1. **Probe** — a generation whose connection selector probes a hop must
//!    dial it. The probe task is spawned at commit and issues its first round
//!    immediately, and the hop belongs to that cycle, so a hop that is never
//!    dialed is a probe task that never ran — which is what a generation
//!    committed with an already-cancelled token looks like.
//! 2. **Spawn** — with no listener live, the config's listener key is new, so
//!    the generation must bind a socket and log its port; sessions through it
//!    relay to the destination the generation names, and the session table
//!    renders them under that destination.
//! 3. **Replace** — the config changes the stream destination while keeping
//!    the listener's `listen_addr` key, so the live listener must keep its
//!    port and adopt the new handler: sessions opened after the commit relay
//!    to the new destination. The commit is witnessed by the listener's own
//!    `Connection handler set` log line, so nothing below races the reload.
//! 4. **Refused preparation** — a config that deserializes but cannot resolve
//!    (`conn_selector` names no selector) must leave the live generation
//!    exactly as it was: the same port still relaying to step 3's
//!    destination, with no handler swap and no new listener.
//! 5. **Sessions in flight** — sessions opened before the reloads keep
//!    serving across them and keep the destination they were opened against.
//! 6. **Retire** — removing the listener from the config must close its
//!    listening socket: a fresh connect to the old port is refused, while a
//!    session already established on it keeps relaying.
//!
//! The soak closes with one more accounting check: every session the cycles
//! established has ended, so the session table must settle to exactly the one
//! session that never did — a table that keeps a finished session's row is
//! still growing at that point, which over a soak is an unbounded leak.
//!
//! Every wait is bounded, and a wait that expires is reported as a failure of
//! a named kind rather than retried: a stall is a finding here, not noise.
//! The kinds separate a config change the serve loop never observed, a
//! process that exited, an observed change whose effect never arrived, a
//! listener that was never retired, and a session that stopped being served.
//!
//! One run is bounded evidence, not proof: with zero failures over N cycles
//! the one-sided 95% upper bound on the per-cycle failure rate is about
//! `3/N`. The summary the test prints states the cycles, the concurrency, the
//! load and that bound, so a clean run can be read as the exclusion it is.
//! Scale is set by `PROXY_SOAK_INSTANCES`, `PROXY_SOAK_CYCLES` and
//! `PROXY_SOAK_BURST`; the defaults keep an ordinary workspace test run
//! short, and the soak is deepened by raising them.

use std::{
    path::PathBuf,
    process::{ExitStatus, Stdio},
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};

use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    net::{TcpListener, TcpStream},
    process::{Child, Command},
    task::JoinSet,
};

/// The example header key the access server's probe chain's hop uses.
const HEADER_KEY: &str = "cHJveHktZXhhbXBsZS1rZXk";

/// Logged by the serve loop for every config change it consumes: the witness
/// that the file watcher's notification reached the reload machine at all.
const CHANGE_LINE: &str = "Config file changed";
/// Logged by a listener when it starts: the witness that a generation bound a
/// new listening socket, and the source of that socket's port.
const LISTEN_LINE: &str = "Listening addr=";
/// Logged by a listener when it adopts a handler for its existing socket: the
/// witness that a reload kept the listener and replaced its handler.
const HANDLER_LINE: &str = "Connection handler set";
/// Logged when a reload could not be prepared; the live generation is
/// untouched.
const PREPARE_FAIL_LINE: &str = "Failed to prepare reload";
/// The monitor logs its bound address behind this text.
const MONITOR_LINE: &str = "listening addr: ";

/// Sessions relay a fixed-size token, and both ends use `read_exact` /
/// `write_all`, so a relay is byte-exact under any TCP segmentation and an
/// echo mismatch is a relay defect rather than a chunking artefact.
const TOKEN_LEN: usize = 16;

/// Every await in a cycle is bounded by one of these. A budget that expires
/// means its effect never happened, which is reported with the budget and the
/// process's own evidence.
#[derive(Debug, Clone, Copy)]
struct Budgets {
    /// The serve loop consuming a config change (its own log line).
    notify: Duration,
    /// A new listener appearing: bind, commit, first poll.
    spawn: Duration,
    /// A live listener adopting a new handler.
    replace: Duration,
    /// A refused preparation being reported.
    prepare_fail: Duration,
    /// A retired listener's socket closing.
    retire: Duration,
    /// One session's connect, request and echo.
    relay: Duration,
    /// The session table rendering a session that is open.
    accounting: Duration,
    /// The session table settling after every session has ended. A finished
    /// session's row is retained for `DEAD_SESSION_RETENTION_DURATION`, so
    /// this bound covers that retention plus the removal itself.
    settle: Duration,
}

impl Budgets {
    fn soak() -> Self {
        Self {
            notify: Duration::from_secs(8),
            spawn: Duration::from_secs(20),
            replace: Duration::from_secs(20),
            prepare_fail: Duration::from_secs(20),
            retire: Duration::from_secs(20),
            relay: Duration::from_secs(20),
            accounting: Duration::from_secs(20),
            settle: Duration::from_secs(40),
        }
    }
}

/// How much soak to run. The defaults are the scale an ordinary workspace
/// test run can afford; the soak is deepened from the environment.
#[derive(Debug, Clone, Copy)]
struct Knobs {
    instances: usize,
    cycles: usize,
    burst: usize,
}

impl Knobs {
    fn from_env() -> Self {
        fn knob(name: &str, default: usize) -> usize {
            std::env::var(name)
                .ok()
                .and_then(|v| v.parse::<usize>().ok())
                .filter(|v| *v > 0)
                .unwrap_or(default)
        }
        Self {
            instances: knob("PROXY_SOAK_INSTANCES", 4),
            cycles: knob("PROXY_SOAK_CYCLES", 2),
            burst: knob("PROXY_SOAK_BURST", 6),
        }
    }

    /// The sessions opened before a reload and checked again after it.
    fn spanning(&self) -> usize {
        (self.burst / 2).max(2)
    }
}

fn proxy_bin() -> &'static str {
    env!("CARGO_BIN_EXE_proxy")
}

fn unique_temp_dir(tag: &str) -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    std::env::temp_dir().join(format!("proxy-soak-{tag}-{}-{nanos}", std::process::id()))
}

/// Strip the ANSI escape sequences the fmt subscriber writes around field
/// names and values, so a log line can be matched on its text alone.
fn strip_ansi(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    let mut chars = line.chars();
    while let Some(c) = chars.next() {
        if c != '\u{1b}' {
            out.push(c);
            continue;
        }
        if chars.next() != Some('[') {
            continue;
        }
        for c in chars.by_ref() {
            if ('\u{40}'..='\u{7e}').contains(&c) {
                break;
            }
        }
    }
    out
}

/// The port a `Listening addr=<socket>` line reports.
fn port_of(listen_line: &str) -> Option<u16> {
    listen_line
        .split(LISTEN_LINE)
        .nth(1)?
        .trim()
        .rsplit(':')
        .next()?
        .parse()
        .ok()
}

/// A config whose only listener routes directly to `dest_port`. `note` makes
/// every write a distinct file content, so no two phases share a config and
/// each write is its own change.
fn config_on(note: &str, dest_port: u16) -> String {
    format!(
        r#"# {note}
[access_server.stream.conn_selector]
"default" = {{ chains = [] }}

[[access_server.tcp_server]]
listen_addr = "127.0.0.1:0"
destination = "tcp://127.0.0.1:{dest_port}"
conn_selector = "default"
"#
    )
}

/// A config with no listener at all: the generation that retires every
/// listener the previous one had.
fn config_off(note: &str) -> String {
    format!("# {note}\n")
}

/// A config that deserializes but cannot resolve: the `conn_selector` it
/// names is not defined, so preparation must fail and the live generation
/// must keep serving.
fn config_bad(note: &str) -> String {
    format!(
        r#"# {note}
[access_server.stream.conn_selector]
"default" = {{ chains = [] }}

[[access_server.tcp_server]]
listen_addr = "127.0.0.1:0"
destination = "tcp://127.0.0.1:9"
conn_selector = "missing"
"#
    )
}

/// A config whose connection selector probes one hop and which has no
/// listener at all: committing it starts a probe task and nothing else, so a
/// dial to that hop is attributable to the generation the commit installed.
fn config_probe(note: &str, hop_port: u16) -> String {
    format!(
        r#"# {note}
[stream.upstream]
"hop" = {{ address = "tcp://127.0.0.1:{hop_port}", header_key = "{HEADER_KEY}" }}

[access_server.stream.conn_selector]
"default" = {{ chains = [{{ weight = 1, chain = ["hop"] }}], probe_rtt = true }}
"#
    )
}

/// A token of exactly [`TOKEN_LEN`] bytes, unique per `(cycle, phase, index)`
/// within an instance, so one arrival is attributable to exactly one send.
fn token(cycle: u64, phase: u64, index: u64) -> [u8; TOKEN_LEN] {
    let mut t = [0u8; TOKEN_LEN];
    t[..4].copy_from_slice(&(cycle as u32).to_be_bytes());
    t[4..8].copy_from_slice(&(phase as u32).to_be_bytes());
    t[8..].copy_from_slice(&index.to_be_bytes());
    t
}

/// A test-owned echo server. It records each token *before* writing the echo,
/// so once a client holds its echo the responder's record of that token is
/// already visible to an assertion.
struct Responder {
    port: u16,
    accepts: Arc<AtomicUsize>,
    received: Arc<Mutex<Vec<[u8; TOKEN_LEN]>>>,
    /// Object-owned: dropping the responder aborts its accept loop, which
    /// owns the connection handlers, so no task outlives the test.
    _tasks: JoinSet<()>,
}

impl Responder {
    /// Bind an ephemeral loopback listener. `bind` chooses a free port, but
    /// that port can be taken between the choice and the bind, so a bounded
    /// number of attempts is made; exhausting them fails loudly rather than
    /// degrading the soak.
    async fn bind() -> Self {
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
/// write error ends the handler: the client side is what reports a failure,
/// so a responder never panics.
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

/// A hop that a generation's route chain probes. It accepts and immediately
/// drops every connection, so a probe round fails fast and the task returns
/// to its interval instead of holding the round open.
struct ProbeHop {
    port: u16,
    dials: Arc<AtomicUsize>,
    _tasks: JoinSet<()>,
}

impl ProbeHop {
    async fn bind() -> Self {
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
        let dials = Arc::new(AtomicUsize::new(0));
        let mut tasks = JoinSet::new();
        {
            let dials = Arc::clone(&dials);
            tasks.spawn(async move {
                while let Ok((stream, _peer)) = listener.accept().await {
                    dials.fetch_add(1, Ordering::SeqCst);
                    drop(stream);
                }
            });
        }
        Self {
            port,
            dials,
            _tasks: tasks,
        }
    }

    /// Wait, bounded, for the hop to be dialed. A generation whose selector
    /// probes this hop dials it as soon as its probe task is spawned at
    /// commit, and the hop belongs to one cycle, so a dial that never arrives
    /// is a probe task that never ran.
    async fn await_dials(&self, want: usize, budget: Duration) -> Result<(), String> {
        let deadline = Instant::now() + budget;
        loop {
            let seen = self.dials.load(Ordering::SeqCst);
            if seen >= want {
                return Ok(());
            }
            if Instant::now() >= deadline {
                return Err(format!(
                    "no probe dial reached the hop at port {} within {budget:?} (saw \
                     {seen}): the generation's probe task never ran, which is what a \
                     generation committed with an already-cancelled token does",
                    self.port
                ));
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }
}

/// Wait, bounded, until `port` refuses a fresh connection: the only witness
/// inside a process that a listener was retired, since a retired listener
/// logs nothing when it goes.
async fn await_closed(port: u16, budget: Duration) -> bool {
    let deadline = Instant::now() + budget;
    while Instant::now() < deadline {
        match TcpStream::connect(("127.0.0.1", port)).await {
            Ok(stream) => {
                drop(stream);
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            Err(_) => return true,
        }
    }
    false
}

/// Where a phase sits, for failure attribution.
#[derive(Debug, Clone, Copy)]
struct PhaseCtx {
    cycle: usize,
    phase: &'static str,
    kind: &'static str,
}

impl PhaseCtx {
    fn new(cycle: usize, phase: &'static str) -> Self {
        Self {
            cycle,
            phase,
            kind: EFFECT_STALL,
        }
    }
    fn with_kind(self, kind: &'static str) -> Self {
        Self { kind, ..self }
    }
}

/// The effect a config write must produce, waited for through the process's
/// own log.
#[derive(Debug, Clone, Copy)]
enum PhaseEffect {
    /// A new listening socket, logged with its port.
    NewListener,
    /// An existing listening socket adopting a new handler.
    HandlerSwap,
    /// A preparation rejected, leaving the live generation serving.
    PrepareRefused,
    /// The commit is witnessed by the served change alone; the effect is
    /// asserted by the caller through something other than a log line (a
    /// socket closing, a probe dialing its hop).
    CommittedByCaller,
}

const NOT_OBSERVED: &str = "CHANGE-NOT-OBSERVED";
const EFFECT_STALL: &str = "EFFECT-STALL";
const PROCESS_EXIT: &str = "PROCESS-EXIT";
const RETIRE_STALL: &str = "RETIRE-STALL";
const PREPARE_REFUSED: &str = "PREPARE-REFUSED";
const RELAY: &str = "RELAY";
const ACCOUNTING: &str = "ACCOUNTING";
const LIVE_SESSION: &str = "LIVE-SESSION";

/// A failed invariant, carrying the cycle and phase it was observed in and
/// the evidence that classifies it.
#[derive(Debug)]
struct Failure {
    cycle: usize,
    phase: &'static str,
    kind: &'static str,
    detail: String,
}

impl Failure {
    fn new(ctx: PhaseCtx, detail: String) -> Self {
        Self {
            cycle: ctx.cycle,
            phase: ctx.phase,
            kind: ctx.kind,
            detail,
        }
    }

    fn in_phase(cycle: usize, phase: &'static str, kind: &'static str, detail: String) -> Self {
        Self {
            cycle,
            phase,
            kind,
            detail,
        }
    }
}

/// The log a process writes, shared between its reader tasks and the
/// assertion helpers.
#[derive(Default)]
struct LogState {
    lines: Vec<String>,
}

/// One running `proxy` process, with the instruments its lifecycle is
/// asserted through: its log, its monitor address, and the config file it
/// watches.
struct Proc {
    child: Child,
    log: Arc<Mutex<LogState>>,
    readers: JoinSet<()>,
    monitor: String,
    config_path: PathBuf,
    dir: PathBuf,
}

impl Drop for Proc {
    fn drop(&mut self) {
        // The process is the test's child; nothing may outlive the test.
        // `kill_on_drop` covers the same case, and this covers a `Proc`
        // dropped on a path that never reaches `shutdown`.
        let _ = self.child.start_kill();
    }
}

impl Proc {
    /// Spawn the binary on `initial` and learn its monitor address from its
    /// own log.
    async fn spawn(name: String, initial: &str, budget: Budgets) -> Result<Self, Failure> {
        let dir = unique_temp_dir(&name);
        std::fs::create_dir_all(&dir).expect("the soak's temp dir must be creatable");
        let config_path = dir.join("config.toml");
        std::fs::write(&config_path, initial).expect("the initial config must be written");

        let mut child = Command::new(proxy_bin())
            .arg(config_path.to_str().unwrap())
            .args(["--monitor-listen-addr", "127.0.0.1:0"])
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .expect("the proxy binary must be spawnable");

        let log = Arc::new(Mutex::new(LogState::default()));
        let mut readers = JoinSet::new();
        let stdout: Box<dyn tokio::io::AsyncRead + Unpin + Send> =
            Box::new(child.stdout.take().unwrap());
        let stderr: Box<dyn tokio::io::AsyncRead + Unpin + Send> =
            Box::new(child.stderr.take().unwrap());
        for stream in [stdout, stderr] {
            let log = Arc::clone(&log);
            readers.spawn(async move {
                let mut lines = BufReader::new(stream).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    log.lock().unwrap().lines.push(strip_ansi(&line));
                }
            });
        }

        let mut proc = Self {
            child,
            log,
            readers,
            monitor: String::new(),
            config_path,
            dir,
        };
        // The monitor address is the one port the test does not choose; the
        // process logs it before serve starts, so this cannot race a reload.
        // It is not a witness that the initial config was read: the initial
        // generation's own effect is, and the caller waits for that.
        let ctx = PhaseCtx::new(0, "startup");
        proc.await_line(MONITOR_LINE, 1, budget.spawn, ctx)
            .await
            .map(|line| {
                proc.monitor = line
                    .split(MONITOR_LINE)
                    .nth(1)
                    .expect("the monitor line carries its address")
                    .trim()
                    .to_owned();
                proc
            })
    }

    fn lines(&self) -> Vec<String> {
        self.log.lock().unwrap().lines.clone()
    }

    fn count(&self, needle: &str) -> usize {
        self.lines().iter().filter(|l| l.contains(needle)).count()
    }

    /// The n-th (1-based) log line containing `needle`.
    fn nth_line(&self, needle: &str, n: usize) -> Option<String> {
        self.lines()
            .into_iter()
            .filter(|l| l.contains(needle))
            .nth(n.saturating_sub(1))
    }

    fn exited(&mut self) -> Option<ExitStatus> {
        self.child.try_wait().ok().flatten()
    }

    /// The evidence a failure is reported with: whether the process is gone
    /// and what it last logged.
    fn evidence(&mut self) -> String {
        let exit = self.exited();
        let lines = self.lines();
        let tail: Vec<&str> = lines
            .iter()
            .rev()
            .take(10)
            .rev()
            .map(String::as_str)
            .collect();
        format!("exit={exit:?}; log tail:\n      {}", tail.join("\n      "))
    }

    fn write_config(&self, src: &str) {
        std::fs::write(&self.config_path, src).expect("the config file must be writable");
    }

    /// Wait, bounded by `budget`, for the log to contain `want` occurrences of
    /// `needle`. A process that exits is reported immediately instead of
    /// waiting out the budget.
    async fn await_count(
        &mut self,
        needle: &str,
        want: usize,
        budget: Duration,
        ctx: PhaseCtx,
    ) -> Result<(), Failure> {
        let deadline = Instant::now() + budget;
        loop {
            if self.count(needle) >= want {
                return Ok(());
            }
            if let Some(exit) = self.exited() {
                return Err(Failure::new(
                    ctx.with_kind(PROCESS_EXIT),
                    format!(
                        "the process exited ({exit:?}) while waiting for {want} line(s) \
                         containing {needle:?}; {}",
                        self.evidence()
                    ),
                ));
            }
            if Instant::now() >= deadline {
                return Err(Failure::new(
                    ctx,
                    format!(
                        "no {want}th line containing {needle:?} within {budget:?} (saw {}); {}",
                        self.count(needle),
                        self.evidence()
                    ),
                ));
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    /// [`Self::await_count`], returning the waited-for line.
    async fn await_line(
        &mut self,
        needle: &str,
        want: usize,
        budget: Duration,
        ctx: PhaseCtx,
    ) -> Result<String, Failure> {
        self.await_count(needle, want, budget, ctx).await?;
        Ok(self
            .nth_line(needle, want)
            .expect("the line that satisfied the wait exists"))
    }

    /// Write `src`, wait for the serve loop to consume the change, then wait
    /// for the phase's own effect. The two waits are separate so a lost
    /// notification and a stalled reload are reported as different kinds.
    async fn apply_config(
        &mut self,
        src: &str,
        effect: PhaseEffect,
        budget: Budgets,
        ctx: PhaseCtx,
    ) -> Result<(), Failure> {
        let before = self.count(CHANGE_LINE);
        self.write_config(src);
        self.await_count(
            CHANGE_LINE,
            before + 1,
            budget.notify,
            ctx.with_kind(NOT_OBSERVED),
        )
        .await?;
        match effect {
            PhaseEffect::NewListener => {
                let before = self.count(LISTEN_LINE);
                self.await_count(LISTEN_LINE, before + 1, budget.spawn, ctx)
                    .await
            }
            PhaseEffect::HandlerSwap => {
                let before = self.count(HANDLER_LINE);
                self.await_count(HANDLER_LINE, before + 1, budget.replace, ctx)
                    .await
            }
            PhaseEffect::PrepareRefused => {
                let before = self.count(PREPARE_FAIL_LINE);
                self.await_count(PREPARE_FAIL_LINE, before + 1, budget.prepare_fail, ctx)
                    .await
            }
            PhaseEffect::CommittedByCaller => Ok(()),
        }
    }

    /// The port of the newest listener the process logged.
    fn newest_listener_port(&self, ctx: PhaseCtx) -> Result<u16, Failure> {
        let line = self
            .nth_line(LISTEN_LINE, self.count(LISTEN_LINE))
            .expect("a listener line was waited for before this call");
        port_of(&line)
            .ok_or_else(|| Failure::new(ctx, format!("a listener line carried no port: {line:?}")))
    }

    async fn shutdown(mut self) {
        self.child.kill().await.ok();
        let _ = self.child.wait().await;
        self.readers.shutdown().await;
        std::fs::remove_dir_all(&self.dir).ok();
    }
}

/// One instance's live state: the process plus the sessions and ports the
/// cycle protocol carries from phase to phase.
struct Soak {
    proc: Proc,
    /// The port of the generation's listener the next cycle drives.
    live_port: u16,
    /// A session opened against the warm-up generation and never closed, so
    /// every later reload is asserted to leave a pre-existing session alone.
    long_lived: TcpStream,
    /// The destination the long-lived session was opened against: its route
    /// is fixed at accept time, so every ping must arrive here.
    long_lived_dest: Responder,
    /// Counters for the run summary.
    reloads: usize,
    sessions: usize,
}

impl Soak {
    /// Spawn the process and bring it to the state every cycle starts from: a
    /// listener bound to `warm.port`, with a long-lived session open against
    /// it.
    ///
    /// The initial config has no listener, so the first committed change
    /// spawns one. That first write is also the run's one chance to hit the
    /// serve loop's subscription window (a notification sent before the loop
    /// subscribes is dropped), so it happens before any assertion depends on
    /// a generation: if it is lost, the run fails as `CHANGE-NOT-OBSERVED`
    /// rather than silently measuring fewer cycles.
    async fn start(name: String, budget: Budgets) -> Result<Self, Failure> {
        // The initial config's own effect is the witness that the process read
        // and committed it: a probe chain with no listener at all, whose hop
        // is dialed as soon as the initial generation's probe task runs. No
        // other config is written until that dial is seen, which is what makes
        // the first write unambiguous — a config written before the initial
        // read would otherwise *be* the initial config, and the write's own
        // effect would never appear.
        let initial_hop = ProbeHop::bind().await;
        let mut proc =
            Proc::spawn(name, &config_probe("initial", initial_hop.port), budget).await?;
        let ctx = PhaseCtx::new(0, "startup");
        if let Err(detail) = initial_hop.await_dials(1, budget.spawn).await {
            let evidence = proc.evidence();
            proc.shutdown().await;
            return Err(Failure::new(ctx, format!("{detail}; {evidence}")));
        }

        let ctx = PhaseCtx::new(0, "warmup-spawn");
        let long_lived_dest = Responder::bind().await;
        let prepare_fails_before = proc.count(PREPARE_FAIL_LINE);
        if let Err(e) = proc
            .apply_config(
                &config_on("warmup spawn", long_lived_dest.port),
                PhaseEffect::NewListener,
                budget,
                ctx,
            )
            .await
        {
            // A generation that could not be bound says so in the process's
            // own log; a spawn that never happened says nothing, so the
            // evidence is what tells the two apart.
            let refused = proc.count(PREPARE_FAIL_LINE) > prepare_fails_before;
            let detail = if refused {
                format!(
                    "the warm-up generation was never bound because its preparation was \
                     refused: {}",
                    proc.nth_line(PREPARE_FAIL_LINE, proc.count(PREPARE_FAIL_LINE))
                        .unwrap_or_default()
                )
            } else {
                e.detail
            };
            let evidence = proc.evidence();
            proc.shutdown().await;
            return Err(Failure {
                cycle: ctx.cycle,
                phase: ctx.phase,
                kind: if refused { PREPARE_REFUSED } else { ctx.kind },
                detail: format!("{detail}; {evidence}"),
            });
        }
        let live_port = match proc.newest_listener_port(ctx) {
            Ok(port) => port,
            Err(e) => {
                proc.shutdown().await;
                return Err(e);
            }
        };
        let warm_token = token(u64::MAX, 0, 0);
        let long_lived = match open_session(live_port, warm_token, budget.relay).await {
            Ok(stream) => stream,
            Err(e) => {
                proc.shutdown().await;
                return Err(Failure {
                    cycle: 0,
                    phase: "warmup-session",
                    kind: RELAY,
                    detail: format!("the warm-up listener did not serve a session: {e}"),
                });
            }
        };
        if !long_lived_dest.saw(&warm_token) {
            proc.shutdown().await;
            return Err(Failure {
                cycle: 0,
                phase: "warmup-session",
                kind: RELAY,
                detail: "the warm-up listener answered a session but its destination never \
                         recorded the token"
                    .to_owned(),
            });
        }
        // The warm-up generation is retired before the first cycle: every
        // cycle starts with no listener, and the long-lived session spans a
        // retirement from its first moment.
        let ctx = PhaseCtx::new(0, "warmup-retire");
        if let Err(e) = proc
            .apply_config(
                &config_off("warmup off"),
                PhaseEffect::CommittedByCaller,
                budget,
                ctx,
            )
            .await
        {
            proc.shutdown().await;
            return Err(e);
        }
        if !await_closed(live_port, budget.retire).await {
            let retire_budget = budget.retire;
            let evidence = proc.evidence();
            proc.shutdown().await;
            return Err(Failure::new(
                ctx.with_kind(RETIRE_STALL),
                format!(
                    "the warm-up listener at port {live_port} still accepted connections \
                     {retire_budget:?} after the config that removes it was consumed: the \
                     retired generation was never retired; {evidence}"
                ),
            ));
        }
        let mut soak = Self {
            proc,
            live_port,
            long_lived,
            long_lived_dest,
            reloads: 2,
            sessions: 1,
        };
        match soak.check_long_lived(0, 1, "warmup-retire", budget).await {
            Ok(()) => Ok(soak),
            Err(e) => {
                soak.proc.shutdown().await;
                Err(e)
            }
        }
    }

    /// Run one cycle, leaving the process with no listener and the long-lived
    /// session still open: the state every cycle starts from. A failure stops
    /// the instance, because every assertion after it depends on the state
    /// this cycle should have left behind.
    async fn cycle(&mut self, cycle: usize, knobs: Knobs, budget: Budgets) -> Result<(), Failure> {
        // (1) Probe: a generation whose connection selector probes a hop must
        // dial it. The probe task is spawned at commit and dials its first
        // round immediately, and the hop belongs to this cycle, so a dial to
        // it can only come from this generation. This is the direction the
        // generation's cancellation token governs: a token cancelled while
        // the generation is being committed leaves the probe task nothing to
        // run, and the hop is never dialed.
        let ctx = PhaseCtx::new(cycle, "probe");
        let hop = ProbeHop::bind().await;
        self.proc
            .apply_config(
                &config_probe(&format!("cycle {cycle} probe"), hop.port),
                PhaseEffect::CommittedByCaller,
                budget,
                ctx,
            )
            .await?;
        self.reloads += 1;
        if let Err(detail) = hop.await_dials(1, budget.spawn).await {
            let evidence = self.proc.evidence();
            return Err(Failure::new(ctx, format!("{detail}; {evidence}")));
        }

        let fresh = Responder::bind().await;
        let next = Responder::bind().await;

        // (2) Spawn: no listener is live, so the config's listener key is new
        // and this generation must bind a socket and log its port.
        let ctx = PhaseCtx::new(cycle, "spawn");
        let prepare_fails_before = self.proc.count(PREPARE_FAIL_LINE);
        if let Err(e) = self
            .proc
            .apply_config(
                &config_on(&format!("cycle {cycle} spawn"), fresh.port),
                PhaseEffect::NewListener,
                budget,
                ctx,
            )
            .await
        {
            return Err(self.classify_spawn(e, prepare_fails_before));
        }
        self.reloads += 1;
        self.live_port = self.proc.newest_listener_port(ctx)?;
        let served = self
            .burst(&fresh, cycle, 5, knobs.burst, "spawn", budget)
            .await?;
        self.sessions += served;
        // The accounting witness is held open across the poll: a session's row
        // is the operator's view of a session that exists, so the poll must
        // not race the row's removal.
        let witness_token = token(cycle as u64, 5, u64::MAX);
        let witness = open_session(self.live_port, witness_token, budget.relay)
            .await
            .map_err(|e| {
                Failure::new(
                    ctx.with_kind(RELAY),
                    format!("the spawned generation did not serve a session: {e}"),
                )
            })?;
        if !fresh.saw(&witness_token) {
            return Err(Failure::new(
                ctx.with_kind(RELAY),
                "the spawned generation answered a session but its destination never \
                 recorded the token"
                    .to_owned(),
            ));
        }
        self.sessions += 1;
        let accounting_ctx = PhaseCtx::new(cycle, "accounting");
        await_accounting(&self.proc.monitor, fresh.port, budget.accounting)
            .await
            .map_err(|detail| Failure::new(accounting_ctx.with_kind(ACCOUNTING), detail))?;
        drop(witness);
        self.check_long_lived(cycle, 6, "spawn", budget).await?;

        // (3) Replace: the destination changes, the listener's key does not,
        // so the live listener must keep its port and adopt the new handler.
        let ctx = PhaseCtx::new(cycle, "replace");
        let listeners_before = self.proc.count(LISTEN_LINE);
        self.proc
            .apply_config(
                &config_on(&format!("cycle {cycle} replace"), next.port),
                PhaseEffect::HandlerSwap,
                budget,
                ctx,
            )
            .await?;
        self.reloads += 1;
        if self.proc.count(LISTEN_LINE) != listeners_before {
            return Err(Failure::new(
                ctx,
                format!(
                    "a handler swap must keep the live listener, but a new listener was \
                     logged ({listeners_before} -> {} listener lines); {}",
                    self.proc.count(LISTEN_LINE),
                    self.proc.evidence()
                ),
            ));
        }
        let served = self
            .burst(&next, cycle, 1, knobs.burst, "replace", budget)
            .await?;
        self.sessions += served;

        // Sessions opened before the reload must still be in flight: this
        // cycle's spanning set is opened now and checked after the refused
        // preparation below.
        let mut spanning = self
            .spanning(&next, cycle, 8, knobs.spanning(), "replace", budget)
            .await?;

        // (4) Refused preparation: a config that cannot resolve must install
        // nothing.
        let ctx = PhaseCtx::new(cycle, "prepare-refused");
        let handlers_before = self.proc.count(HANDLER_LINE);
        let listeners_before = self.proc.count(LISTEN_LINE);
        self.proc
            .apply_config(
                &config_bad(&format!("cycle {cycle} refused")),
                PhaseEffect::PrepareRefused,
                budget,
                ctx,
            )
            .await?;
        self.reloads += 1;
        if self.proc.count(HANDLER_LINE) != handlers_before
            || self.proc.count(LISTEN_LINE) != listeners_before
        {
            return Err(Failure::new(
                ctx,
                format!(
                    "a refused preparation must install nothing, but the listener set \
                     changed (handler lines {handlers_before} -> {}, listener lines \
                     {listeners_before} -> {}); {}",
                    self.proc.count(HANDLER_LINE),
                    self.proc.count(LISTEN_LINE),
                    self.proc.evidence()
                ),
            ));
        }
        let served = self
            .burst(&next, cycle, 2, knobs.burst, "prepare-refused", budget)
            .await?;
        self.sessions += served;
        self.ping(&mut spanning, &next, cycle, 9, "prepare-refused", budget)
            .await?;

        // (5) The long-lived session must still reach the destination it was
        // opened against, after this cycle's commits, one of them a handler
        // swap.
        self.check_long_lived(cycle, 3, "long-lived", budget)
            .await?;

        // (6) Retire: removing the listener must close its socket, and a
        // session already established on it must keep relaying.
        let ctx = PhaseCtx::new(cycle, "retire");
        let retired = self.live_port;
        self.proc
            .apply_config(
                &config_off(&format!("cycle {cycle} off")),
                PhaseEffect::CommittedByCaller,
                budget,
                ctx,
            )
            .await?;
        self.reloads += 1;
        if !await_closed(retired, budget.retire).await {
            let retire_budget = budget.retire;
            return Err(Failure::new(
                ctx.with_kind(RETIRE_STALL),
                format!(
                    "the listener at port {retired} still accepted connections \
                     {retire_budget:?} after the config that removes it was consumed; the \
                     only witness of that commit is the close itself, so either the \
                     retired generation was never retired or the reload never \
                     committed: {}",
                    self.proc.evidence()
                ),
            ));
        }
        self.check_long_lived(cycle, 4, "retire", budget).await?;
        drop(spanning);
        Ok(())
    }

    /// A spawn whose new listener never appeared: a generation whose bind
    /// failed is logged by the process as a refused preparation, and that is
    /// not a reload path that stalled — it is a generation that could not be
    /// created (on a loaded host, loopback ephemeral-port exhaustion makes
    /// `bind("127.0.0.1:0")` fail). The cycle still fails; only the attributed
    /// kind changes, so the report says which of the two it was.
    fn classify_spawn(&mut self, failure: Failure, prepare_fails_before: usize) -> Failure {
        if self.proc.count(PREPARE_FAIL_LINE) > prepare_fails_before {
            let line = self
                .proc
                .nth_line(PREPARE_FAIL_LINE, self.proc.count(PREPARE_FAIL_LINE))
                .unwrap_or_default();
            return Failure {
                kind: PREPARE_REFUSED,
                detail: format!(
                    "no listener was bound because the generation's preparation was \
                     refused: {line}; original observation: {}",
                    failure.detail
                ),
                ..failure
            };
        }
        failure
    }

    /// The soak's closing accounting check: every session the cycles
    /// established has ended, so the table must settle to exactly the one
    /// session that never did. A table that keeps a finished session's row is
    /// still growing at this point — over a soak that is an unbounded leak —
    /// and a table that never records a row renders the same empty view as a
    /// runtime wired with no table at all.
    async fn settle_accounting(&self, cycles: usize, budget: Budgets) -> Result<(), Failure> {
        let ctx = PhaseCtx::new(cycles, "accounting-settled");
        await_only_open_session(&self.proc.monitor, self.long_lived_dest.port, budget.settle)
            .await
            .map_err(|detail| Failure::new(ctx.with_kind(ACCOUNTING), detail))
    }

    /// Relay one token on the session opened against the warm-up generation
    /// and require it to arrive at the destination that session was opened
    /// against: a reload must not re-point or divert a session that is
    /// already in flight.
    async fn check_long_lived(
        &mut self,
        cycle: usize,
        phase_no: u64,
        phase: &'static str,
        budget: Budgets,
    ) -> Result<(), Failure> {
        let token = token(cycle as u64, phase_no, 0);
        if let Err(e) = send_and_echo(&mut self.long_lived, token, budget.relay).await {
            let evidence = self.proc.evidence();
            return Err(Failure::in_phase(
                cycle,
                phase,
                LIVE_SESSION,
                format!(
                    "a session opened before the reloads failed to relay after {} of \
                     them: {e}; {evidence}",
                    self.reloads
                ),
            ));
        }
        if !self.long_lived_dest.saw(&token) {
            return Err(Failure::in_phase(
                cycle,
                phase,
                LIVE_SESSION,
                "a session opened before the reloads was echoed without reaching the \
                 destination it was routed to: a reload re-pointed or diverted a live \
                 session"
                    .to_owned(),
            ));
        }
        Ok(())
    }

    /// `count` sessions opened concurrently against the live listener, each
    /// attributed to `responder`.
    async fn burst(
        &mut self,
        responder: &Responder,
        cycle: usize,
        phase: u64,
        count: usize,
        label: &'static str,
        budget: Budgets,
    ) -> Result<usize, Failure> {
        burst_sessions(self.live_port, responder, cycle, phase, count, budget)
            .await
            .map_err(|detail| {
                let evidence = self.proc.evidence();
                Failure::in_phase(cycle, label, RELAY, format!("{detail}; {evidence}"))
            })
    }

    async fn spanning(
        &mut self,
        responder: &Responder,
        cycle: usize,
        phase: u64,
        count: usize,
        label: &'static str,
        budget: Budgets,
    ) -> Result<Vec<TcpStream>, Failure> {
        let mut streams = Vec::with_capacity(count);
        for index in 0..count {
            let token = token(cycle as u64, phase, index as u64);
            let stream = open_session(self.live_port, token, budget.relay)
                .await
                .map_err(|e| {
                    let evidence = self.proc.evidence();
                    Failure::in_phase(
                        cycle,
                        label,
                        RELAY,
                        format!("a session of the spanning set failed: {e}; {evidence}"),
                    )
                })?;
            if !responder.saw(&token) {
                return Err(Failure::in_phase(
                    cycle,
                    label,
                    RELAY,
                    "a spanning session was echoed without reaching the destination the \
                     current generation routes to"
                        .to_owned(),
                ));
            }
            streams.push(stream);
        }
        Ok(streams)
    }

    async fn ping(
        &mut self,
        streams: &mut [TcpStream],
        responder: &Responder,
        cycle: usize,
        phase: u64,
        label: &'static str,
        budget: Budgets,
    ) -> Result<(), Failure> {
        for (index, stream) in streams.iter_mut().enumerate() {
            let token = token(cycle as u64, phase, index as u64);
            send_and_echo(stream, token, budget.relay)
                .await
                .map_err(|e| {
                    Failure::in_phase(
                        cycle,
                        label,
                        LIVE_SESSION,
                        format!(
                            "a session opened before the reload failed to relay after \
                                 it: {e}"
                        ),
                    )
                })?;
            if !responder.saw(&token) {
                return Err(Failure::in_phase(
                    cycle,
                    label,
                    LIVE_SESSION,
                    "a session opened before the reload was echoed without reaching the \
                     destination it was routed to: the reload re-pointed or diverted a \
                     live session"
                        .to_owned(),
                ));
            }
        }
        Ok(())
    }
}

/// Wait, bounded, for the session table to render a stream session whose
/// destination is `dest_port`.
async fn await_accounting(monitor: &str, dest_port: u16, budget: Duration) -> Result<(), String> {
    let needle = dest_port.to_string();
    let deadline = Instant::now() + budget;
    let mut last = String::new();
    loop {
        if let Ok(response) = http_get(monitor, "/sessions").await {
            let (stream_rows, _) = session_blocks(&response);
            if stream_rows
                .iter()
                .any(|row| row.split_whitespace().any(|cell| cell == needle))
            {
                return Ok(());
            }
            last = response;
        }
        if Instant::now() >= deadline {
            return Err(format!(
                "the session table never rendered a stream session with destination port \
                 {dest_port} within {budget:?}; last /sessions response: {last:?}"
            ));
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// Wait, bounded, for the session table to render exactly the sessions that
/// are still open, and require that set to be the one session whose
/// destination is `open_destination`: every other session the cycle
/// established has ended, so its row must be gone. A table that keeps a row
/// after its session ends grows without bound over a soak, and one that never
/// records a row renders the same empty view as a runtime wired with no table
/// at all.
async fn await_only_open_session(
    monitor: &str,
    open_destination: u16,
    budget: Duration,
) -> Result<(), String> {
    let needle = open_destination.to_string();
    let deadline = Instant::now() + budget;
    let mut last = String::new();
    loop {
        if let Ok(response) = http_get(monitor, "/sessions").await {
            let (stream_rows, _) = session_blocks(&response);
            // The header row plus exactly one session row.
            if stream_rows.len() == 2
                && stream_rows[1].split_whitespace().any(|cell| cell == needle)
            {
                return Ok(());
            }
            last = response;
        }
        if Instant::now() >= deadline {
            return Err(format!(
                "the session table did not settle to exactly the one open session with \
                 destination port {open_destination} within {budget:?}; last /sessions \
                 response: {last:?}"
            ));
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// The two session blocks a `/sessions` response renders, each block's
/// non-blank lines with its header row first. This is the operator's view, so
/// the accounting assertion reads exactly what an operator reads.
fn session_blocks(response: &str) -> (Vec<&str>, Vec<&str>) {
    let body = response.split("\r\n\r\n").nth(1).unwrap_or("");
    let after_stream = body
        .split_once("Stream:")
        .map(|(_, rest)| rest)
        .unwrap_or("");
    let (stream, udp) = after_stream
        .split_once("UDP:")
        .unwrap_or((after_stream, ""));
    fn lines(block: &str) -> Vec<&str> {
        block.lines().filter(|l| !l.trim().is_empty()).collect()
    }
    (lines(stream), lines(udp))
}

async fn http_get(monitor: &str, path: &str) -> std::io::Result<String> {
    let (host, port) = monitor
        .rsplit_once(':')
        .expect("the monitor address is host:port");
    let mut stream = TcpStream::connect((host, port.parse::<u16>().unwrap())).await?;
    let request = format!("GET {path} HTTP/1.1\r\nHost: monitor\r\nConnection: close\r\n\r\n");
    stream.write_all(request.as_bytes()).await?;
    let mut response = String::new();
    stream.read_to_string(&mut response).await?;
    Ok(response)
}

/// Connect to `port`, send `token` and require the echo `E:` followed by that
/// token. The returned stream is the open session, ready for more tokens, so
/// a caller can keep it alive across reloads.
async fn open_session(
    port: u16,
    token: [u8; TOKEN_LEN],
    budget: Duration,
) -> std::io::Result<TcpStream> {
    let mut stream = tokio::time::timeout(budget, TcpStream::connect(("127.0.0.1", port)))
        .await
        .map_err(|_| std::io::Error::other("connect timed out"))??;
    send_and_echo(&mut stream, token, budget).await?;
    Ok(stream)
}

/// Send one token on an open session and require its echo.
async fn send_and_echo(
    stream: &mut TcpStream,
    token: [u8; TOKEN_LEN],
    budget: Duration,
) -> std::io::Result<()> {
    let mut expected = [0u8; TOKEN_LEN + 2];
    expected[..2].copy_from_slice(b"E:");
    expected[2..].copy_from_slice(&token);
    tokio::time::timeout(budget, async {
        stream.write_all(&token).await?;
        let mut got = [0u8; TOKEN_LEN + 2];
        stream.read_exact(&mut got).await?;
        if got != expected {
            return Err(std::io::Error::other("the echo did not match the token"));
        }
        Ok(())
    })
    .await
    .map_err(|_| std::io::Error::other("relay timed out"))?
}

/// `count` sessions opened concurrently against `port`, each requiring its
/// echo and each attributed to `responder`.
async fn burst_sessions(
    port: u16,
    responder: &Responder,
    cycle: usize,
    phase: u64,
    count: usize,
    budget: Budgets,
) -> Result<usize, String> {
    let mut tasks = JoinSet::new();
    for index in 0..count {
        let token = token(cycle as u64, phase, index as u64);
        tasks.spawn(async move {
            let stream = open_session(port, token, budget.relay).await?;
            drop(stream);
            Ok::<_, std::io::Error>(token)
        });
    }
    let mut tokens = Vec::with_capacity(count);
    while let Some(joined) = tasks.join_next().await {
        match joined {
            Ok(Ok(token)) => tokens.push(token),
            Ok(Err(e)) => return Err(format!("a burst session failed: {e}")),
            Err(e) => return Err(format!("a burst session task failed: {e}")),
        }
    }
    for token in &tokens {
        if !responder.saw(token) {
            return Err(format!(
                "a burst session was echoed without reaching the destination the current \
                 generation routes to ({} connections accepted there)",
                responder.accepts()
            ));
        }
    }
    Ok(tokens.len())
}

/// What one instance's soak run achieved.
#[derive(Debug)]
struct Report {
    instance: String,
    cycles: usize,
    reloads: usize,
    sessions: usize,
    failure: Option<Failure>,
}

async fn run_instance(name: String, knobs: Knobs, budget: Budgets) -> Report {
    let mut soak = match Soak::start(name.clone(), budget).await {
        Ok(soak) => soak,
        Err(failure) => {
            return Report {
                instance: name,
                cycles: 0,
                reloads: 0,
                sessions: 0,
                failure: Some(failure),
            };
        }
    };
    let mut cycles = 0;
    let mut failure = None;
    for cycle in 0..knobs.cycles {
        match soak.cycle(cycle, knobs, budget).await {
            Ok(()) => cycles += 1,
            Err(e) => {
                failure = Some(e);
                break;
            }
        }
    }
    if failure.is_none()
        && let Err(e) = soak.settle_accounting(cycles, budget).await
    {
        failure = Some(e);
    }
    let reloads = soak.reloads;
    let sessions = soak.sessions;
    soak.proc.shutdown().await;
    Report {
        instance: name,
        cycles,
        reloads,
        sessions,
        failure,
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn the_lifecycle_holds_across_repeated_reloads() {
    let knobs = Knobs::from_env();
    let budget = Budgets::soak();
    let started = Instant::now();

    let mut tasks = JoinSet::new();
    for index in 0..knobs.instances {
        tasks.spawn(run_instance(format!("soak-{index}"), knobs, budget));
    }
    let mut reports = Vec::new();
    let mut panics = Vec::new();
    while let Some(joined) = tasks.join_next().await {
        match joined {
            Ok(report) => reports.push(report),
            Err(error) => panics.push(error.to_string()),
        }
    }
    let elapsed = started.elapsed();

    let cycles: usize = reports.iter().map(|r| r.cycles).sum();
    let reloads: usize = reports.iter().map(|r| r.reloads).sum();
    let sessions: usize = reports.iter().map(|r| r.sessions).sum();
    let failures: Vec<&Failure> = reports.iter().filter_map(|r| r.failure.as_ref()).collect();

    let bound = if cycles == 0 {
        None
    } else {
        Some(3.0 / cycles as f64)
    };
    let mut summary = format!(
        "proxy lifecycle soak: instances={} cycles/instance={} burst={} spanning={} => \
         {cycles} cycles, {reloads} reloads, {sessions} sessions in {elapsed:?}",
        knobs.instances,
        knobs.cycles,
        knobs.burst,
        knobs.spanning(),
    );
    // A detection bound is only a bound for a run that completed cleanly; a
    // run that failed says what failed instead.
    if failures.is_empty() && panics.is_empty() && cycles == knobs.instances * knobs.cycles {
        summary.push_str(&format!(
            "; a 0-failure run excludes a per-cycle failure rate above {:.4} at 95% \
             (rule of three)",
            bound.expect("a clean run with cycles has a bound")
        ));
    } else {
        summary.push_str("; the run did not complete cleanly, so it states no detection bound");
    }
    eprintln!("{summary}");
    for report in &reports {
        eprintln!(
            "  {}: cycles={} reloads={} sessions={} {}",
            report.instance,
            report.cycles,
            report.reloads,
            report.sessions,
            match &report.failure {
                None => "ok".to_owned(),
                Some(f) => format!(
                    "FAILED cycle {} phase {} kind {}: {}",
                    f.cycle, f.phase, f.kind, f.detail
                ),
            }
        );
    }

    assert!(
        panics.is_empty(),
        "soak instance tasks panicked: {panics:?}"
    );
    assert!(
        failures.is_empty(),
        "the lifecycle soak found {} failure(s) in {cycles} cycles: {:#?}",
        failures.len(),
        failures
    );
    assert_eq!(
        cycles,
        knobs.instances * knobs.cycles,
        "every instance must complete every cycle; a run that stopped early is reported \
         above with its failure"
    );
}
