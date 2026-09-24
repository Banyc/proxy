//! Pins the line the serve loop writes when a reload cannot be prepared.
//!
//! A configuration can deserialize and still fail to resolve, which is the
//! one preparation failure a running server can be handed: `prepare_reload`
//! returns `ServerServeError::Load` and the serve loop must report it, keep
//! the live configuration, and stay ready for the next change. The line the
//! loop writes is the only diagnostic an operator gets for that state, and it
//! is only useful if it carries the loader's own error — the message alone
//! says a reload failed, while the payload says which key of which
//! configuration the operator has to fix.
//!
//! The assertion is made through a subscriber the test installs, not through
//! the process's stdout, so it reads the event the loop actually emits rather
//! than a rendering of it. This file holds exactly one test: a global
//! subscriber can only be installed once per process.

use std::{
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
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

/// One access-server TCP listener with a selector that resolves. The
/// listener binds an ephemeral loopback port, so the test never races one.
const VALID: &str = r#"
[access_server.stream.conn_selector]
"default" = { chains = [] }

[[access_server.tcp_server]]
listen_addr = "127.0.0.1:0"
destination = "tcp://127.0.0.1:9"
conn_selector = "default"
"#;

/// The same configuration with the selector renamed: it deserializes, and
/// resolution fails because `missing` names no selector. The error must name
/// that key.
const UNRESOLVABLE: &str = r#"
[access_server.stream.conn_selector]
"default" = { chains = [] }

[[access_server.tcp_server]]
listen_addr = "127.0.0.1:0"
destination = "tcp://127.0.0.1:9"
conn_selector = "missing"
"#;

/// The message of the serve loop's own error line for a failed preparation.
const PREPARE_FAIL_MESSAGE: &str = "Failed to prepare reload; live config unchanged";

/// The module the serve loop's events are emitted from.
const SERVE_TARGET: &str = "server";

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

/// Renders every event's target and fields into a buffer, so the test reads
/// the event the serve loop emits. Process-global: the loop runs on the
/// runtime's threads, not the test's.
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

/// Every rendered event line that is the serve loop's failed-preparation
/// error, in the order the events landed.
fn prepare_fail_lines(buf: &Arc<Mutex<Vec<u8>>>) -> Vec<String> {
    rendered(buf)
        .lines()
        .filter(|line| line.starts_with(&format!("[{SERVE_TARGET}] ")))
        .filter(|line| line.contains(PREPARE_FAIL_MESSAGE))
        .map(str::to_owned)
        .collect()
}

// -- driving the serve loop -----------------------------------------------------------

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
    tokio::time::timeout(Duration::from_secs(30), rx.recv())
        .await
        .expect("timed out waiting for the serve loop to read a config")
}

/// A change notification is broadcast over a `watch` generation counter and
/// is only observed by a subscriber that already exists. The serve loop
/// subscribes *after* its initial commit, so a notification sent in that
/// window is dropped. Retry the change until the serve loop's next config
/// read lands on the channel; the channel, not the delay, is the success
/// signal.
async fn await_next_read(
    rx: &mut tokio::sync::mpsc::Receiver<usize>,
    config_changed: &ConfigChangeSignal,
    expected: usize,
) {
    for _ in 0..10 {
        config_changed.notify_waiters();
        match tokio::time::timeout(Duration::from_secs(2), rx.recv()).await {
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

/// A preparation that cannot resolve is reported once, with the loader's
/// error attached, and the loop keeps serving.
///
/// The reader serves a valid configuration, then one whose `conn_selector`
/// names no selector, then a valid one again. The failed preparation is the
/// only event the loop emits for it, so the count is exact: a line that is
/// dropped, duplicated, or emitted for a successful preparation fails here.
/// The payload is what an operator acts on — the name of the key that could
/// not be resolved — so it is asserted, not just the message.
#[tokio::test(flavor = "multi_thread")]
async fn a_failed_preparation_is_reported_once_with_its_cause_and_the_loop_survives() {
    let events = capture_events();

    let (reads_tx, mut reads_rx) = tokio::sync::mpsc::channel(8);
    let reader = ScriptedReader::new(
        vec![
            VALID.to_string(),
            UNRESOLVABLE.to_string(),
            VALID.to_string(),
        ],
        reads_tx,
    );
    let config_changed = ConfigChangeSignal::new();
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

    // The initial generation is read and committed.
    assert_eq!(
        recv_config(&mut reads_rx).await,
        Some(0),
        "serve must read the initial config"
    );
    assert!(
        prepare_fail_lines(&events).is_empty(),
        "a valid initial configuration must not report a failed preparation: {}",
        rendered(&events)
    );

    // The unresolvable configuration is read, and its preparation fails.
    await_next_read(&mut reads_rx, &config_changed, 1).await;
    // The loop's report follows the read; wait for it instead of racing it.
    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    while prepare_fail_lines(&events).is_empty() {
        assert!(
            std::time::Instant::now() < deadline,
            "the serve loop never reported the failed preparation within 30s; captured events:\n{}",
            rendered(&events)
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    let lines = prepare_fail_lines(&events);
    assert_eq!(
        lines.len(),
        1,
        "one failed preparation must be reported exactly once; captured events:\n{}",
        rendered(&events)
    );
    let line = &lines[0];
    assert!(
        line.contains("e=Load("),
        "the report must carry the failed preparation's error, not just its message: {line}"
    );
    assert!(
        line.contains("missing"),
        "the report must name the configuration key the operator has to fix: {line}"
    );

    // The live configuration is unchanged and the loop keeps serving: the
    // next change is still read and applied.
    await_next_read(&mut reads_rx, &config_changed, 2).await;
    assert!(
        tasks.try_join_next().is_none(),
        "the serve loop must still be running after a failed preparation"
    );

    tasks.shutdown().await;
}
