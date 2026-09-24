//! End-to-end exercise of the production serve loop through its config
//! reader: the initial generation is prepared and committed, a config change
//! drives a reload, and a reload whose preparation fails must not kill the
//! server — the next change is still read and applied.
//!
//! Every listener binds `127.0.0.1:0`, so the test never races a fixed port
//! and never needs to know the chosen port: it observes the reload lifecycle
//! through the config reader's read channel instead of a socket.

use std::sync::atomic::{AtomicUsize, Ordering};

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

/// A valid server config covering every access-server and proxy-server kind,
/// with all listeners on an ephemeral port.
const VALID: &str = r#"
[stream.upstream]
"hop1" = { address = "tcp://127.0.0.1:9", header_key = "cHJveHktZXhhbXBsZS1rZXk" }

[udp.upstream]
"uhop1" = { address = "127.0.0.1:9", header_key = "cHJveHktZXhhbXBsZS1rZXk" }

[access_server.stream.conn_selector]
"default" = { chains = [{ weight = 1, chain = ["hop1"] }], probe_rtt = false }

[access_server.udp.conn_selector]
"default" = { chains = [{ weight = 1, chain = ["uhop1"] }], probe_rtt = false }

[access_server.stream.route_table]
"direct" = [{ matcher = {}, action = "direct" }]

[[access_server.tcp_server]]
listen_addr = "127.0.0.1:0"
destination = "tcp://127.0.0.1:9"
conn_selector = "default"

[[access_server.udp_server]]
listen_addr = "127.0.0.1:0"
destination = "127.0.0.1:9"
conn_selector = "default"

[[access_server.http_server]]
listen_addr = "127.0.0.1:0"
route_table = "direct"

[[access_server.socks5_tcp_server]]
listen_addr = "127.0.0.1:0"
route_table = "direct"

[[access_server.socks5_udp_server]]
listen_addr = "127.0.0.1:0"
conn_selector = "default"

[[proxy_server.tcp_server]]
listen_addr = "127.0.0.1:0"
header_key = "cHJveHktZXhhbXBsZS1rZXk"
allow_loopback = true

[[proxy_server.tcp_mux_server]]
listen_addr = "127.0.0.1:0"
header_key = "cHJveHktZXhhbXBsZS1rZXk"
allow_loopback = true

[[proxy_server.udp_server]]
listen_addr = "127.0.0.1:0"
header_key = "cHJveHktZXhhbXBsZS1rZXk"
allow_loopback = true

[[proxy_server.kcp_server]]
listen_addr = "127.0.0.1:0"
header_key = "cHJveHktZXhhbXBsZS1rZXk"
allow_loopback = true

[[proxy_server.mptcp_server]]
listen_addr = "127.0.0.1:0"
header_key = "cHJveHktZXhhbXBsZS1rZXk"
allow_loopback = true

[[proxy_server.rtp_server]]
listen_addr = "127.0.0.1:0"
header_key = "cHJveHktZXhhbXBsZS1rZXk"
allow_loopback = true

[[proxy_server.rtp_mux_server]]
listen_addr = "127.0.0.1:0"
header_key = "cHJveHktZXhhbXBsZS1rZXk"
allow_loopback = true
"#;

/// A config that deserializes but whose route resolution fails: every
/// `conn_selector = "default"` reference names a key that is not defined.
fn invalid_conn_selector_config() -> String {
    VALID.replace("conn_selector = \"default\"", "conn_selector = \"missing\"")
}

/// A [`ReadConfig`] that serves a scripted sequence of TOML configs (the last
/// one repeating) and reports each read index on a channel, so a test can
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

#[tokio::test(flavor = "multi_thread")]
async fn a_reload_is_applied_and_a_failed_prepare_does_not_kill_the_server() {
    let (reads_tx, mut reads_rx) = tokio::sync::mpsc::channel(8);
    let reader = ScriptedReader::new(
        vec![
            VALID.to_string(),
            invalid_conn_selector_config(),
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

    // Bound every await so a wedged serve loop fails instead of hanging.
    async fn recv_config(rx: &mut tokio::sync::mpsc::Receiver<usize>) -> Option<usize> {
        tokio::time::timeout(std::time::Duration::from_secs(30), rx.recv())
            .await
            .expect("timed out waiting for the serve loop to read a config")
    }

    // A change notification is broadcast over a `watch` generation counter
    // and is only observed by a subscriber that already exists. The serve
    // loop subscribes *after* its initial commit, so a notification sent in
    // that window is dropped. Retry the change until the serve loop's next
    // config read lands on the channel; the channel, not the delay, is the
    // success signal.
    async fn await_next_read(
        rx: &mut tokio::sync::mpsc::Receiver<usize>,
        config_changed: &ConfigChangeSignal,
        expected: usize,
    ) {
        for _ in 0..10 {
            config_changed.notify_waiters();
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

    // The initial generation is read and committed; every listener binds.
    assert_eq!(
        recv_config(&mut reads_rx).await,
        Some(0),
        "serve must read the initial config"
    );

    // A config change starts a reload; this generation's preparation fails
    // (an undefined conn_selector), and the serve loop must survive it.
    await_next_read(&mut reads_rx, &config_changed, 1).await;

    // A later change must still be read and applied: the failed preparation
    // returned the machine to idle rather than terminating the server.
    await_next_read(&mut reads_rx, &config_changed, 2).await;
    assert!(
        tasks.try_join_next().is_none(),
        "the serve loop must still be running after a failed reload preparation"
    );

    tasks.shutdown().await;
}

/// The reader really is the production input path: a config that cannot be
/// deserialized must surface as a read error, not be silently accepted.
#[tokio::test]
async fn a_malformed_config_surfaces_as_a_read_error() {
    let (reads_tx, _reads_rx) = tokio::sync::mpsc::channel(1);
    let reader = ScriptedReader::new(vec!["this is not toml = = =".to_string()], reads_tx);
    let result = reader.read_config().await;
    assert!(
        result.is_err(),
        "a malformed config must fail to read, got {result:?}"
    );
}

/// A config that names one `listen_addr` twice in one kind. The key is the
/// address, so the two entries cannot both serve: accepting them binds two
/// sockets, spawns two listeners and drops one of them, leaving an address the
/// startup log reports and nothing accepts on.
const DUPLICATE_LISTEN_ADDR: &str = r#"
[[proxy_server.tcp_server]]
listen_addr = "127.0.0.1:0"
header_key = "cHJveHktZXhhbXBsZS1rZXk"
allow_loopback = true

[[proxy_server.tcp_server]]
listen_addr = "127.0.0.1:0"
header_key = "cHJveHktZXhhbXBsZS1rZXk"
allow_loopback = true
"#;

/// The startup path refuses such a config with the address to drop on the
/// error line, instead of starting a server that serves one address twice over.
#[tokio::test(flavor = "multi_thread")]
async fn a_config_naming_one_listen_addr_twice_in_one_kind_is_refused() {
    let (reads_tx, _reads_rx) = tokio::sync::mpsc::channel(1);
    let reader = ScriptedReader::new(vec![DUPLICATE_LISTEN_ADDR.to_string()], reads_tx);
    let (retention_actor, retention) = RetentionActor::new();
    let context = ServeContext {
        stream_session_table: None,
        udp_session_table: None,
        config_changed: ConfigChangeSignal::new(),
        system_resume: SystemResumeSignal(Notify::new()),
        retention,
    };
    let mut tasks = tokio::task::JoinSet::new();
    tasks.spawn(async move {
        let _exit = retention_actor.run().await;
    });
    let mut serve_task: tokio::task::JoinSet<Result<(), server::ServerServeError>> =
        tokio::task::JoinSet::new();
    serve_task.spawn(async move { serve(reader, context).await });

    // Bounded, so a server that starts instead of refusing is reported as a
    // failure of the refusal rather than as a hang.
    let exit = match tokio::time::timeout(
        std::time::Duration::from_secs(10),
        serve_task.join_next(),
    )
    .await
    {
        Ok(Some(result)) => result.expect("the serve task must not panic"),
        Ok(None) => panic!("the serve task set ended without a result"),
        Err(_) => panic!(
            "a config naming one listen_addr twice must be refused at startup: the server kept \
             serving instead of reporting it"
        ),
    };
    let error = match exit {
        Ok(()) => panic!("the server must refuse a config that names one listen_addr twice"),
        Err(error) => error,
    };
    assert!(
        error
            .to_string()
            .contains("duplicate listener configuration key `127.0.0.1:0`"),
        "the refusal an operator reads must name the duplicated address; got: {error}"
    );

    tasks.shutdown().await;
}
