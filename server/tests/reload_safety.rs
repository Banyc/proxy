//! What the running `proxy` process must do when the *file* its config is read
//! from changes underneath it.
//!
//! The process holds every session the operator's client has — the client
//! multiplexes its traffic over one long-lived mux session — so a reload that
//! drops a session costs a cold establishment the client cannot get back, and
//! a reload that crashes the process costs every session at once. The config
//! file is written by tooling this process does not own, so the reload path has
//! to be safe against every shape a writer can leave on disk: bytes that do not
//! parse, bytes that parse but cannot resolve, a path that is missing, a
//! directory, a file it may not read, an atomic replacement, a burst of writes,
//! and a *truncated* file.
//!
//! ## The contract each test pins
//!
//! - **Reject and retain.** A config the running process cannot read, parse or
//!   resolve is reported once and the live generation is left exactly as it
//!   was: the same listener on the same socket, the same destination routing,
//!   and every session already in flight still served.
//! - **Apply without dropping.** A config that resolves is committed in place:
//!   a listener whose key is unchanged keeps its socket (so no connect is ever
//!   refused) and adopts the new handler, while a session opened before the
//!   reload keeps serving and keeps the destination it was routed to.
//! - **The watcher's own path.** Deleting the config file, replacing it with a
//!   directory, or making it unreadable must not stop the service, and the
//!   watcher must keep watching the path so a later valid write is applied.
//!   Replacing the file atomically (rename over it — the common deployment
//!   shape) is a normal successful reload.
//! - **A storm is one reload of the last complete file.** Two writes in quick
//!   succession collapse into a single reload that reads the second; a
//!   truncate followed immediately by a write never applies the truncated
//!   state.
//!
//! ## The truncate window is the dangerous one
//!
//! A writer that truncates a file and then writes it leaves a **zero-byte**
//! file on disk between the two syscalls. Read in that window, the file parses
//! as the default configuration, which has no listeners at all: applying it
//! retires every listener and drops every session on them, and the write that
//! follows then restores a listener **nobody is connected to**. The debounce
//! collapses a truncate and a write that arrive close together, but a writer
//! that holds the file empty for longer than the window — a stalled or aborted
//! deployment, a slow filesystem — leaves the empty state to be read on its
//! own. `a_truncated_config_file_is_refused_and_the_live_session_survives`
//! pins the behaviour that closes that window.
//!
//! Every wait here is bounded and reported as the phase that was waiting, so a
//! stall names itself instead of hanging; every spawned process is killed on
//! drop, so a failed assertion does not leak one.

use std::{
    path::PathBuf,
    process::Stdio,
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

fn proxy_bin() -> &'static str {
    env!("CARGO_BIN_EXE_proxy")
}

/// Every await in a test is bounded by one of these. A budget that expires is
/// reported as a failure of the named phase rather than retried.
#[derive(Debug, Clone, Copy)]
struct Budgets {
    /// The serve loop consuming a config change and reaching a decision
    /// (reload applied or preparation refused). The debounce window is 1 s and
    /// the observation follows it.
    decide: Duration,
    /// One session's connect, request and echo through the process.
    relay: Duration,
}

impl Budgets {
    fn reload() -> Self {
        Self {
            decide: Duration::from_secs(15),
            relay: Duration::from_secs(15),
        }
    }
}

fn unique_temp_dir(tag: &str) -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    std::env::temp_dir().join(format!(
        "proxy-reload-safety-{tag}-{}-{nanos}",
        std::process::id()
    ))
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

/// Logged by the serve loop for every config change it consumes.
const CHANGE_LINE: &str = "Config file changed";
/// Logged by a listener when it binds a socket: a new listener, and the source
/// of its port.
const LISTEN_LINE: &str = "Listening addr=";
/// Logged by a listener when it adopts a handler for its existing socket: a
/// reload kept the listener and replaced its handler.
const HANDLER_LINE: &str = "Connection handler set";
/// Logged when a reload could not be prepared; the live generation is
/// untouched.
const PREPARE_FAIL_LINE: &str = "Failed to prepare reload";

/// Sessions relay a fixed-size token and both ends use `read_exact` /
/// `write_all`, so a relay is byte-exact under any TCP segmentation.
const TOKEN_LEN: usize = 16;

/// A token of exactly [`TOKEN_LEN`] bytes, unique per `(phase, index)` within
/// a test, so one arrival is attributable to exactly one send.
fn token(phase: u8, index: u8) -> [u8; TOKEN_LEN] {
    let mut t = [0u8; TOKEN_LEN];
    t[0] = phase;
    t[1] = index;
    t[2] = phase;
    t[3] = index;
    t
}

/// A test-owned echo server. It records each token *before* writing the echo,
/// so once a client holds its echo the responder's record of that token is
/// already visible to an assertion.
struct Responder {
    port: u16,
    received: Arc<Mutex<Vec<[u8; TOKEN_LEN]>>>,
    /// Object-owned: dropping the responder aborts its accept loop, which owns
    /// the connection handlers, so no task outlives the test.
    _tasks: JoinSet<()>,
}

impl Responder {
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
        let received = Arc::new(Mutex::new(Vec::new()));
        let mut tasks = JoinSet::new();
        {
            let received = Arc::clone(&received);
            tasks.spawn(async move {
                let mut handlers: JoinSet<()> = JoinSet::new();
                loop {
                    tokio::select! {
                        accepted = listener.accept() => match accepted {
                            Ok((stream, _peer)) => {
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
            received,
            _tasks: tasks,
        }
    }

    fn saw(&self, token: &[u8; TOKEN_LEN]) -> bool {
        self.received.lock().unwrap().contains(token)
    }
}

/// Read fixed-size tokens on one connection and echo each one.
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

/// One running `proxy` process, with the only two instruments its reload
/// lifecycle is read through: its own log, and its config file.
struct Proxy {
    child: Child,
    log: Arc<Mutex<Vec<String>>>,
    readers: JoinSet<()>,
    config_path: PathBuf,
    dir: PathBuf,
    /// The address of the access-server listener the initial generation bound.
    listen_addr: String,
}

impl Drop for Proxy {
    fn drop(&mut self) {
        // The process is the test's child; nothing may outlive the test.
        let _ = self.child.start_kill();
    }
}

impl Proxy {
    /// Spawn the binary on `initial` and learn its listener address from its
    /// own log. `initial` must bind exactly one access-server listener.
    async fn start(name: &str, initial: &str, budget: Budgets) -> Self {
        let dir = unique_temp_dir(name);
        std::fs::create_dir_all(&dir).expect("the test's temp dir must be creatable");
        let config_path = dir.join("config.toml");
        std::fs::write(&config_path, initial).expect("the initial config must be written");

        let mut child = Command::new(proxy_bin())
            .arg(config_path.to_str().unwrap())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .expect("the proxy binary must be spawnable");

        let log = Arc::new(Mutex::new(Vec::new()));
        let mut readers = JoinSet::new();
        for stream in [
            Box::new(child.stdout.take().unwrap()) as Box<dyn tokio::io::AsyncRead + Unpin + Send>,
            Box::new(child.stderr.take().unwrap()),
        ] {
            let log = Arc::clone(&log);
            readers.spawn(async move {
                let mut lines = BufReader::new(stream).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    log.lock().unwrap().push(strip_ansi(&line));
                }
            });
        }

        let mut proxy = Self {
            child,
            log,
            readers,
            config_path,
            dir,
            listen_addr: String::new(),
        };
        let deadline = Instant::now() + budget.decide;
        loop {
            if let Some(line) = proxy.nth_line(LISTEN_LINE, 1) {
                proxy.listen_addr = line
                    .split(LISTEN_LINE)
                    .nth(1)
                    .expect("the listener line carries its address")
                    .trim()
                    .to_owned();
                return proxy;
            }
            if let Some(exit) = proxy.exited() {
                panic!(
                    "the process exited ({exit:?}) before binding its listener; log:\n{}",
                    proxy.log_text()
                );
            }
            if Instant::now() >= deadline {
                panic!(
                    "the process never logged a listener within {:?}; log:\n{}",
                    budget.decide,
                    proxy.log_text()
                );
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    fn port(&self) -> u16 {
        self.listen_addr
            .rsplit(':')
            .next()
            .and_then(|p| p.parse().ok())
            .unwrap_or_else(|| {
                panic!(
                    "the listener address carries no port: {:?}",
                    self.listen_addr
                )
            })
    }

    fn lines(&self) -> Vec<String> {
        self.log.lock().unwrap().clone()
    }

    fn log_text(&self) -> String {
        self.lines().join("\n")
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

    fn exited(&mut self) -> Option<std::process::ExitStatus> {
        self.child.try_wait().ok().flatten()
    }

    /// Whether the process is still running (reaping it if it has exited).
    fn alive(&mut self) -> bool {
        self.exited().is_none()
    }

    /// The evidence a failure is reported with: whether the process is gone and
    /// what it last logged.
    fn evidence(&mut self) -> String {
        let exit = self.exited();
        let lines = self.lines();
        let tail: Vec<&str> = lines
            .iter()
            .rev()
            .take(12)
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
        phase: &str,
    ) -> Result<(), String> {
        let deadline = Instant::now() + budget;
        loop {
            if self.count(needle) >= want {
                return Ok(());
            }
            if let Some(exit) = self.exited() {
                return Err(format!(
                    "{phase}: the process exited ({exit:?}) while waiting for {want} line(s) \
                     containing {needle:?}; {}",
                    self.evidence()
                ));
            }
            if Instant::now() >= deadline {
                return Err(format!(
                    "{phase}: no {want}th line containing {needle:?} within {budget:?} (saw {}); {}",
                    self.count(needle),
                    self.evidence()
                ));
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    /// Wait for the serve loop's *next* failed preparation. Used when the
    /// change that drives it is made by something other than
    /// [`Self::write_config`] — deleting or chmod-ing the path, say.
    async fn await_prepare_fail(&mut self, budget: Budgets, phase: &str) -> Result<(), String> {
        let before = self.count(PREPARE_FAIL_LINE);
        self.await_count(
            PREPARE_FAIL_LINE,
            before + 1,
            budget.decide,
            &format!("{phase}: the bad reload must be reported, not applied"),
        )
        .await
    }

    /// Wait for the live listener to adopt its next handler. Used when the
    /// change is made by restoring the path rather than by writing it.
    async fn await_handler_swap(&mut self, budget: Budgets, phase: &str) -> Result<(), String> {
        let before = self.count(HANDLER_LINE);
        self.await_count(
            HANDLER_LINE,
            before + 1,
            budget.decide,
            &format!("{phase}: the live listener must adopt the new handler"),
        )
        .await
    }

    /// Write `src`, wait for the serve loop to consume the change, then require
    /// the preparation to be refused.
    async fn expect_refused(
        &mut self,
        src: &str,
        budget: Budgets,
        phase: &str,
    ) -> Result<(), String> {
        let before = self.count(CHANGE_LINE);
        self.write_config(src);
        self.await_count(
            CHANGE_LINE,
            before + 1,
            budget.decide,
            &format!("{phase}: the config change must reach the serve loop"),
        )
        .await?;
        self.await_prepare_fail(budget, phase).await
    }

    /// Write `src`, wait for the serve loop to consume the change, then require
    /// the live listener to adopt a new handler.
    async fn expect_handler_swap(
        &mut self,
        src: &str,
        budget: Budgets,
        phase: &str,
    ) -> Result<(), String> {
        let before = self.count(CHANGE_LINE);
        self.write_config(src);
        self.await_count(
            CHANGE_LINE,
            before + 1,
            budget.decide,
            &format!("{phase}: the config change must reach the serve loop"),
        )
        .await?;
        self.await_handler_swap(budget, phase).await
    }

    async fn shutdown(mut self) {
        self.child.kill().await.ok();
        let _ = self.child.wait().await;
        self.readers.shutdown().await;
        std::fs::remove_dir_all(&self.dir).ok();
    }
}

/// A config whose only listener routes directly to `dest_port`. `note` makes
/// every write a distinct file content, so no two phases share a config and
/// each write is its own change. `listen_addr` is the *string* `127.0.0.1:0`,
/// so a reload keeps the same listener key and must adopt the handler on the
/// socket the first generation bound.
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

/// The four shapes of an invalid reload the brief names, each of which must be
/// refused with the live generation retained.
fn invalid_configs() -> Vec<(&'static str, String)> {
    vec![
        // Bytes that do not parse as TOML at all.
        ("bad toml", "[access_server\n".to_owned()),
        // A schema-invalid field: the top-level struct denies unknown fields.
        ("unknown field", "nonsense_field = 1\n".to_owned()),
        // A missing required field: the listener has no destination.
        (
            "missing field",
            "[access_server.stream.conn_selector]\n\"default\" = { chains = [] }\n\
             [[access_server.tcp_server]]\nlisten_addr = \"127.0.0.1:0\"\n\
             conn_selector = \"default\"\n"
                .to_owned(),
        ),
        // A bad address: it deserializes as a string and fails to resolve.
        (
            "bad address",
            "[access_server.stream.conn_selector]\n\"default\" = { chains = [] }\n\
             [[access_server.tcp_server]]\nlisten_addr = \"127.0.0.1:0\"\n\
             destination = \"not an address\"\nconn_selector = \"default\"\n"
                .to_owned(),
        ),
    ]
}

/// Connect to `port`, send `token` and require the echo `E:` followed by that
/// token. The returned stream is the open session, ready for more tokens, so a
/// caller can keep it alive across reloads.
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

/// Put `path` back to a normal file holding `content`: undo a directory, a
/// zero-permission mode, or a deletion.
fn restore_path(path: &PathBuf, content: &str) {
    if path.is_dir() {
        std::fs::remove_dir_all(path).ok();
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o644)).ok();
    }
    std::fs::write(path, content).expect("the config file must be restorable");
}

/// A reload of a config that cannot be read, parsed or resolved must be refused
/// with the live generation retained, and the service must keep serving.
///
/// Each of the four invalid shapes is driven through the process's own watcher
/// in turn. For every one the test requires, from the process's own log and its
/// own socket: a failed preparation is reported (so the file was read and
/// judged, not ignored), the process is still running, no listener was bound or
/// retired, the session opened before the bad reload still relays, and a *fresh*
/// connection to the live port is still accepted.
///
/// The assertion is the retention, not the error text: a reload path that
/// dropped the live generation and started an empty one would satisfy "a
/// failure was reported" while failing the socket checks, which is the
/// production incident this test exists to catch.
#[tokio::test(flavor = "multi_thread")]
async fn an_invalid_reload_is_refused_and_the_live_generation_keeps_serving() {
    let budget = Budgets::reload();
    let responder = Responder::bind().await;
    let mut proxy = Proxy::start("invalid", &config_on("initial", responder.port), budget).await;
    let port = proxy.port();

    // A session opened before any bad reload must still be served after all of
    // them.
    let mut live = open_session(port, token(1, 0), budget.relay)
        .await
        .expect("the initial generation must serve a session");
    assert!(
        responder.saw(&token(1, 0)),
        "the initial session was echoed without reaching its destination"
    );

    for (index, (label, src)) in invalid_configs().into_iter().enumerate() {
        let phase = format!("invalid reload ({label})");
        let listeners_before = proxy.count(LISTEN_LINE);
        let handlers_before = proxy.count(HANDLER_LINE);

        proxy
            .expect_refused(&src, budget, &phase)
            .await
            .unwrap_or_else(|e| panic!("{e}"));

        assert!(
            proxy.alive(),
            "{phase}: the process must survive a refused reload; {}",
            proxy.evidence()
        );
        assert_eq!(
            proxy.count(LISTEN_LINE),
            listeners_before,
            "{phase}: a refused reload must not bind a listener; {}",
            proxy.evidence()
        );
        assert_eq!(
            proxy.count(HANDLER_LINE),
            handlers_before,
            "{phase}: a refused reload must not replace a handler; {}",
            proxy.evidence()
        );

        // The session opened before the bad reload is still in flight...
        let t = token(2, index as u8);
        send_and_echo(&mut live, t, budget.relay)
            .await
            .unwrap_or_else(|e| {
                panic!(
                    "{phase}: a session opened before the refused reload stopped being served: \
                     {e}; {}",
                    proxy.evidence()
                )
            });
        assert!(
            responder.saw(&t),
            "{phase}: the live session was echoed without reaching the destination it was \
             routed to; {}",
            proxy.evidence()
        );

        // ...and the listener still accepts new connections.
        let t = token(3, index as u8);
        let fresh = open_session(port, t, budget.relay)
            .await
            .unwrap_or_else(|e| {
                panic!(
                    "{phase}: the live listener refused a fresh connection after a refused reload: \
                 {e}; {}",
                    proxy.evidence()
                )
            });
        drop(fresh);
        assert!(
            responder.saw(&t),
            "{phase}: a fresh session after the refused reload did not reach the destination; {}",
            proxy.evidence()
        );
    }

    // The live generation is *the original one*, not a silent replacement: a
    // valid config that changes the destination is committed as a handler swap
    // on the listener's own socket, not as a new bind.
    let listeners_after = proxy.count(LISTEN_LINE);
    let other = Responder::bind().await;
    proxy
        .expect_handler_swap(
            &config_on("after the bad reloads", other.port),
            budget,
            "restore after refused reloads",
        )
        .await
        .unwrap();
    assert_eq!(
        proxy.count(LISTEN_LINE),
        listeners_after,
        "the listener must move to the new handler on its own socket, not be rebound; {}",
        proxy.evidence()
    );
    let t = token(4, 0);
    let mut after = open_session(port, t, budget.relay)
        .await
        .unwrap_or_else(|e| {
            panic!(
                "the listener must still serve after the bad reloads and the restore: {e}; {}",
                proxy.evidence()
            )
        });
    assert!(
        other.saw(&t),
        "a session opened after the restore must reach the restored destination; {}",
        proxy.evidence()
    );
    send_and_echo(&mut after, token(5, 0), budget.relay)
        .await
        .expect("the restored generation must keep serving the fresh session");

    proxy.shutdown().await;
}

/// A *valid* reload must swap the handler in place: no connection is ever
/// refused, and no session is dropped.
///
/// The assertion is made by hammering the live listener with fresh connections
/// across the reload window while a long-lived session stays open. The listener
/// key is unchanged by the reload, so the loader must adopt the new handler on
/// the socket the first generation bound; if it closed and rebound the socket,
/// the connect loop records refusals and the long-lived session dies. A single
/// refusal is a failure — the operator's client pays it as a session loss.
#[tokio::test(flavor = "multi_thread")]
async fn a_valid_reload_never_refuses_a_connection_and_keeps_the_live_session() {
    let budget = Budgets::reload();
    let before = Responder::bind().await;
    let after = Responder::bind().await;
    let mut proxy = Proxy::start("valid", &config_on("initial", before.port), budget).await;
    let port = proxy.port();

    let mut live = open_session(port, token(1, 0), budget.relay)
        .await
        .expect("the initial generation must serve a session");

    // A connect loop that runs across the reload window. It records every
    // failure; a listener that is unbound for even one connect is caught.
    let mut loop_tasks: JoinSet<()> = JoinSet::new();
    let failures = Arc::new(AtomicUsize::new(0));
    let attempts = Arc::new(AtomicUsize::new(0));
    {
        let failures = Arc::clone(&failures);
        let attempts = Arc::clone(&attempts);
        loop_tasks.spawn(async move {
            loop {
                attempts.fetch_add(1, Ordering::SeqCst);
                match open_session(port, token(9, 9), Duration::from_secs(5)).await {
                    Ok(stream) => drop(stream),
                    Err(_) => {
                        failures.fetch_add(1, Ordering::SeqCst);
                    }
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        });
    }

    // Let the loop get going before the reload, so the window is measured on
    // both sides of the commit.
    while attempts.load(Ordering::SeqCst) < 20 {
        tokio::time::sleep(Duration::from_millis(5)).await;
    }

    proxy
        .expect_handler_swap(&config_on("reloaded", after.port), budget, "valid reload")
        .await
        .unwrap();

    // Give the loop a little time after the commit so a socket that was closed
    // late would still be observed.
    let settled = attempts.load(Ordering::SeqCst) + 20;
    while attempts.load(Ordering::SeqCst) < settled {
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    loop_tasks.abort_all();

    let attempts = attempts.load(Ordering::SeqCst);
    let failures = failures.load(Ordering::SeqCst);
    // The sample count is part of the evidence: a pass that measured nothing
    // must not read as a pass.
    eprintln!(
        "reload window: {attempts} connections attempted across the reload, {failures} refused or \
         failed"
    );
    assert!(
        attempts >= 40,
        "the connect loop must have measured the window on both sides of the commit, but only \
         {attempts} connection(s) were attempted"
    );
    assert_eq!(
        failures,
        0,
        "{failures} of {attempts} connections were refused or failed across a reload that kept \
         the listener's key: a reload must adopt the handler on the live socket, not close and \
         rebind it; {}",
        proxy.evidence()
    );

    // The session opened before the reload is still served, and still reaches
    // the destination it was routed to.
    let t = token(2, 0);
    send_and_echo(&mut live, t, budget.relay)
        .await
        .unwrap_or_else(|e| {
            panic!(
                "the pre-reload session stopped being served: {e}; {}",
                proxy.evidence()
            )
        });
    assert!(
        before.saw(&t),
        "the pre-reload session was echoed without reaching the destination it was routed to: a \
         reload re-pointed or diverted a live session; {}",
        proxy.evidence()
    );

    // A session opened after the reload reaches the *new* destination, so the
    // swap really happened rather than the reload being a no-op.
    let t = token(3, 0);
    let mut fresh = open_session(port, t, budget.relay).await.unwrap();
    assert!(
        after.saw(&t),
        "a session opened after the reload did not reach the reloaded destination; {}",
        proxy.evidence()
    );
    send_and_echo(&mut fresh, token(4, 0), budget.relay)
        .await
        .expect("the reloaded generation must keep serving");

    proxy.shutdown().await;
}

/// The config *path* itself can be made unusable without the process being
/// told to stop: deleted, replaced by a directory, or made unreadable. Each
/// must leave the live generation serving, and the watcher must keep watching
/// the path so that restoring a valid file is applied.
///
/// The re-arm half is what makes this a test of the watcher rather than of the
/// process: a watcher that stopped on the first failure would leave the
/// service frozen on the last good config, and the restore would never be
/// applied.
#[tokio::test(flavor = "multi_thread")]
async fn the_watcher_survives_every_failure_mode_of_its_config_path() {
    let budget = Budgets::reload();
    let responder = Responder::bind().await;
    let mut proxy = Proxy::start("watcher", &config_on("initial", responder.port), budget).await;
    let port = proxy.port();
    let path = proxy.config_path.clone();

    let mut live = open_session(port, token(1, 0), budget.relay)
        .await
        .expect("the initial generation must serve a session");

    // Every restore destination is kept alive for the whole test: a
    // destination that was dropped while the live config still routed to it
    // would make a later fresh connection fail for a reason other than the
    // case under test.
    let mut keep_destinations_alive: Vec<Responder> = Vec::new();

    // Barrier: the watcher must be **armed** before any case makes the path
    // unusable. `spawn_watch_tasks` starts the watcher thread, but the watch
    // itself is established asynchronously, so a path removed before the
    // arming finishes is a different case — the absent-at-arming one, pinned
    // by `server/src/config/mod.rs::
    // a_config_path_that_is_absent_at_arming_is_re_armed_not_reported_as_fatal`.
    // A committed handler swap is observable proof that this process's watcher
    // has delivered a change, and therefore that it is armed.
    {
        let armed = Responder::bind().await;
        let src = config_on("arming the watcher", armed.port);
        keep_destinations_alive.push(armed);
        proxy
            .expect_handler_swap(&src, budget, "arming the watcher")
            .await
            .unwrap();
    }

    // Each case: make the path unusable, require the live generation to be
    // retained, then restore a valid config and require the watcher to deliver
    // it. The restore targets a fresh responder, so the handler really changes
    // and the swap is observable.
    type BreakPath = Box<dyn Fn(&PathBuf) -> bool + Send + Sync>;
    let cases: Vec<(&str, BreakPath)> = vec![
        (
            "deleted",
            Box::new(|path: &PathBuf| std::fs::remove_file(path).is_ok()),
        ),
        (
            "replaced by a directory",
            Box::new(|path: &PathBuf| {
                std::fs::remove_file(path).is_ok() && std::fs::create_dir(path).is_ok()
            }),
        ),
        (
            "made unreadable",
            Box::new(|path: &PathBuf| {
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o000)).is_ok()
                }
                #[cfg(not(unix))]
                {
                    let _ = path;
                    false
                }
            }),
        ),
    ];

    for (index, (label, break_path)) in cases.into_iter().enumerate() {
        let phase = format!("config path {label}");
        assert!(break_path(&path), "{phase}: the case could not be set up");

        let destination = Responder::bind().await;
        let restore_src = config_on(&format!("restored after {label}"), destination.port);
        keep_destinations_alive.push(destination);

        // A privileged runner (root) can read a 0o000 file, and a missing file
        // and a directory are readable-as-errors in every case; only the
        // permission case can be unreachable. If the test itself can read the
        // file, the refusal this case exists to observe cannot be produced.
        if std::fs::read_to_string(&path).is_ok() {
            eprintln!(
                "  skipped `{label}`: this runner can read the file, so the refusal is not \
                 reachable"
            );
            // The restore write is itself the change; its effect is awaited
            // below.
            restore_path(&path, &restore_src);
        } else {
            proxy
                .await_prepare_fail(budget, &phase)
                .await
                .unwrap_or_else(|e| panic!("{e}"));

            assert!(
                proxy.alive(),
                "{phase}: the process must survive an unusable config path; {}",
                proxy.evidence()
            );
            // The listener must still accept, and the held session must still
            // relay: an unusable path must not touch the live generation.
            let t = token(2, index as u8);
            let fresh = open_session(port, t, budget.relay)
                .await
                .unwrap_or_else(|e| {
                    panic!(
                        "{phase}: the live listener stopped accepting connections: {e}; {}",
                        proxy.evidence()
                    )
                });
            drop(fresh);
            send_and_echo(&mut live, token(3, index as u8), budget.relay)
                .await
                .unwrap_or_else(|e| {
                    panic!(
                        "{phase}: the live session stopped being served: {e}; {}",
                        proxy.evidence()
                    )
                });

            // The watcher must still be watching: restoring the file is
            // delivered and applied, as a handler swap on the live socket.
            restore_path(&path, &restore_src);
        }

        let listeners_before = proxy.count(LISTEN_LINE);
        proxy
            .await_handler_swap(budget, &format!("{phase} (restore)"))
            .await
            .unwrap();
        assert_eq!(
            proxy.count(LISTEN_LINE),
            listeners_before,
            "{phase}: the restore must adopt a handler, not rebind the listener; {}",
            proxy.evidence()
        );
    }

    proxy.shutdown().await;
}

/// Replacing the config file atomically — write a sibling, then rename it over
/// the watched path — is the common deployment shape and must be applied as a
/// normal reload that keeps the live socket and the live session.
#[tokio::test(flavor = "multi_thread")]
async fn an_atomically_replaced_config_file_is_applied_and_keeps_the_live_session() {
    let budget = Budgets::reload();
    let before = Responder::bind().await;
    let after = Responder::bind().await;
    let mut proxy = Proxy::start("rename", &config_on("initial", before.port), budget).await;
    let port = proxy.port();

    let mut live = open_session(port, token(1, 0), budget.relay)
        .await
        .expect("the initial generation must serve a session");

    let listeners_before = proxy.count(LISTEN_LINE);
    let staged = proxy.dir.join("config.toml.new");
    std::fs::write(&staged, config_on("renamed over", after.port)).unwrap();
    let changes_before = proxy.count(CHANGE_LINE);
    std::fs::rename(&staged, &proxy.config_path).expect("the rename must succeed");
    proxy
        .await_count(
            CHANGE_LINE,
            changes_before + 1,
            budget.decide,
            "atomic replacement: the rename must signal the watcher",
        )
        .await
        .unwrap();
    proxy
        .await_handler_swap(budget, "atomic replacement")
        .await
        .unwrap();

    assert_eq!(
        proxy.count(LISTEN_LINE),
        listeners_before,
        "an atomic replacement must adopt the handler on the live socket, not rebind; {}",
        proxy.evidence()
    );
    let t = token(2, 0);
    let mut fresh = open_session(port, t, budget.relay).await.unwrap();
    assert!(
        after.saw(&t),
        "a session opened after the atomic replacement must reach the new destination; {}",
        proxy.evidence()
    );
    send_and_echo(&mut live, token(3, 0), budget.relay)
        .await
        .unwrap_or_else(|e| {
            panic!(
                "the session opened before the atomic replacement stopped being served: {e}; {}",
                proxy.evidence()
            )
        });
    assert!(
        before.saw(&token(3, 0)),
        "the pre-replacement session was echoed without reaching the destination it was routed \
         to; {}",
        proxy.evidence()
    );
    send_and_echo(&mut fresh, token(4, 0), budget.relay)
        .await
        .expect("the replaced generation must keep serving");

    proxy.shutdown().await;
}

/// Two writes in quick succession must collapse into a single reload that
/// reads the **second** file, and a truncate followed by a write must never
/// apply the truncated state.
///
/// The truncate half is the one that matters: `std::fs::write` truncates and
/// then writes, and the debounce window is what stops the watcher reading the
/// zero-byte file between the two syscalls. The test asserts the two
/// consequences separately: only the last write is the serving config (the
/// first write's destination receives nothing), and the listener is never
/// retired (the same socket serves a session opened across the storm, and no
/// new listener is logged).
#[tokio::test(flavor = "multi_thread")]
async fn a_reload_storm_collapses_to_the_last_write_and_never_applies_the_truncated_file() {
    let budget = Budgets::reload();
    let first = Responder::bind().await;
    let second = Responder::bind().await;
    let mut proxy = Proxy::start("storm", &config_on("initial", first.port), budget).await;
    let port = proxy.port();

    let mut live = open_session(port, token(1, 0), budget.relay)
        .await
        .expect("the initial generation must serve a session");

    // Two valid writes, 100 ms apart: the debounce must collapse them and the
    // served config must be the second.
    let listeners_before = proxy.count(LISTEN_LINE);
    let changes_before = proxy.count(CHANGE_LINE);
    proxy.write_config(&config_on("storm a", first.port));
    tokio::time::sleep(Duration::from_millis(100)).await;
    proxy.write_config(&config_on("storm b", second.port));
    proxy
        .await_count(
            CHANGE_LINE,
            changes_before + 1,
            budget.decide,
            "storm: the writes must reach the serve loop",
        )
        .await
        .unwrap();
    proxy.await_handler_swap(budget, "storm").await.unwrap();

    let t = token(2, 0);
    let mut fresh = open_session(port, t, budget.relay).await.unwrap();
    assert!(
        second.saw(&t),
        "the storm's last write must be the serving config; {}",
        proxy.evidence()
    );
    send_and_echo(&mut fresh, token(3, 0), budget.relay)
        .await
        .expect("the storm's generation must keep serving");
    assert!(
        !first.saw(&t),
        "the storm's first write must not be the serving config: a reload per write would have \
         routed the session there; {}",
        proxy.evidence()
    );

    // A truncate followed by a write 100 ms later: the debounce must keep the
    // zero-byte state from ever being applied.
    proxy.write_config("");
    tokio::time::sleep(Duration::from_millis(100)).await;
    proxy.write_config(&config_on("after truncate", second.port));
    proxy
        .await_handler_swap(budget, "truncate storm")
        .await
        .unwrap();
    assert_eq!(
        proxy.count(LISTEN_LINE),
        listeners_before,
        "the zero-byte state of a truncate-then-write must never be applied: applying it retires \
         the listener and the write that follows rebinds a socket nobody is connected to; {}",
        proxy.evidence()
    );
    assert!(
        proxy.alive(),
        "the process must survive the storm; {}",
        proxy.evidence()
    );
    send_and_echo(&mut live, token(4, 0), budget.relay)
        .await
        .unwrap_or_else(|e| {
            panic!(
                "the session opened before the storm stopped being served: {e}; {}",
                proxy.evidence()
            )
        });

    proxy.shutdown().await;
}

/// A config file that reads as **zero bytes** must be refused, not applied.
///
/// A zero-byte file is what a writer's truncate leaves on disk before its
/// write, and it parses as the default configuration, which has no listeners
/// at all. Applying it retires every listener and drops every session on them,
/// and the write that follows restores a listener nobody is connected to. The
/// live generation must instead be retained, exactly as for any other config
/// the process cannot accept.
///
/// The test holds a session open across the truncation and requires: the
/// preparation to be *reported as refused*, the process to stay alive, a fresh
/// connection on the live port to be accepted, the held session to keep
/// relaying, and the later restore to adopt a handler on the **same socket**
/// rather than bind a new one — the last of which is what proves the listener
/// was never retired.
///
/// The vacuity demonstration: removing the guard that refuses a zero-byte read
/// turns this test red, because the truncation is then applied and the fresh
/// connection on the live port is refused.
#[tokio::test(flavor = "multi_thread")]
async fn a_truncated_config_file_is_refused_and_the_live_session_survives() {
    let budget = Budgets::reload();
    let responder = Responder::bind().await;
    let mut proxy = Proxy::start("truncated", &config_on("initial", responder.port), budget).await;
    let port = proxy.port();

    let mut live = open_session(port, token(1, 0), budget.relay)
        .await
        .expect("the initial generation must serve a session");

    proxy
        .expect_refused("", budget, "truncated config file")
        .await
        .unwrap_or_else(|e| panic!("{e}"));

    assert!(
        proxy.alive(),
        "the process must survive a truncated config file; {}",
        proxy.evidence()
    );

    // The listener must still be bound *now*: if the zero-byte config had been
    // applied, this connect would be refused.
    let t = token(2, 0);
    let fresh = open_session(port, t, budget.relay).await.unwrap_or_else(|e| {
        panic!(
            "the live listener was retired by a truncated config file, so a fresh connection was \
             refused: a zero-byte file must be refused like any other unreadable config; {e}; {}",
            proxy.evidence()
        )
    });
    drop(fresh);
    assert!(
        responder.saw(&t),
        "a fresh connection after the truncation must reach the destination; {}",
        proxy.evidence()
    );
    send_and_echo(&mut live, token(3, 0), budget.relay)
        .await
        .unwrap_or_else(|e| {
            panic!(
                "the session held across the truncation stopped being served: the zero-byte \
                 config was applied; {e}; {}",
                proxy.evidence()
            )
        });

    // The restore must be a handler swap on the same socket, not a new bind.
    let listeners_before = proxy.count(LISTEN_LINE);
    proxy
        .expect_handler_swap(
            &config_on("restored after truncation", responder.port),
            budget,
            "restore after truncation",
        )
        .await
        .unwrap();
    assert_eq!(
        proxy.count(LISTEN_LINE),
        listeners_before,
        "the restore after a refused truncation must adopt a handler on the live socket: a new \
         bind means the truncation retired the listener; {}",
        proxy.evidence()
    );

    proxy.shutdown().await;
}

/// The listener's port must be the one the initial generation bound, for every
/// test above: `config_on` keeps `listen_addr = "127.0.0.1:0"`, so a reload
/// that bound a new socket would be observable as a new `Listening` line. This
/// is a guard on the shared fixture, not a product assertion: if the fixture
/// ever moved the port, the "no new bind" assertions would be vacuous.
#[test]
fn the_fixture_config_keeps_the_listener_key_across_reloads() {
    let a = config_on("one", 1111);
    let b = config_on("two", 2222);
    let key = |src: &str| {
        src.lines()
            .find(|l| l.starts_with("listen_addr"))
            .map(str::to_owned)
            .expect("the fixture config names a listen address")
    };
    assert_eq!(
        key(&a),
        key(&b),
        "every fixture config must keep the same listener key, or a reload would rebind"
    );
}
