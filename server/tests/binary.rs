//! Exercise the `proxy` binary as a process: the CLI surface (`--help`, the
//! no-config error), the running server with its monitoring HTTP server and
//! CSV record directory, and the session tables that server's `/sessions`
//! view reads. The binary is located through cargo's `CARGO_BIN_EXE_proxy`,
//! so the test drives the real production entry point.
//!
//! Every listener binds `127.0.0.1:0`; the test reads the actual ports from
//! the process's own startup log, so it never races a fixed port. Every
//! spawned process is `kill_on_drop`, so a failed assertion does not leak it.

use std::{process::Stdio, time::Duration};

use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    net::{TcpListener, TcpStream, UdpSocket},
};

fn proxy_bin() -> &'static str {
    env!("CARGO_BIN_EXE_proxy")
}

fn unique_temp_dir(tag: &str) -> std::path::PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    std::env::temp_dir().join(format!(
        "proxy-bin-test-{tag}-{}-{nanos}",
        std::process::id()
    ))
}

#[tokio::test]
async fn help_lists_the_cli_arguments() {
    let output = tokio::process::Command::new(proxy_bin())
        .arg("--help")
        .output()
        .await
        .unwrap();
    assert!(output.status.success(), "--help must succeed");
    let text = String::from_utf8_lossy(&output.stdout);
    assert!(
        text.contains("CONFIG_FILE_PATHS"),
        "--help must document the config paths: {text}"
    );
    assert!(
        text.contains("MONITOR_LISTEN_ADDR"),
        "--help must document the monitor listener: {text}"
    );
}

#[tokio::test]
async fn no_config_files_exits_with_an_error() {
    let output = tokio::process::Command::new(proxy_bin())
        .output()
        .await
        .unwrap();
    assert!(!output.status.success(), "no config must be fatal");
    assert_eq!(output.status.code(), Some(1));
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        text.contains("No config files provided"),
        "the failure must name the missing config: {text}"
    );
}

async fn http_get(addr: &str, path: &str) -> String {
    let mut stream = TcpStream::connect(addr).await.unwrap();
    let request = format!("GET {path} HTTP/1.1\r\nHost: monitor\r\nConnection: close\r\n\r\n");
    stream.write_all(request.as_bytes()).await.unwrap();
    let mut response = String::new();
    tokio::time::timeout(
        Duration::from_secs(10),
        stream.read_to_string(&mut response),
    )
    .await
    .expect("timed out reading the monitor response")
    .unwrap();
    response
}

#[tokio::test(flavor = "multi_thread")]
async fn the_process_serves_the_monitor_routes_and_writes_records() {
    let dir = unique_temp_dir("monitor");
    std::fs::create_dir_all(&dir).unwrap();
    let config_path = dir.join("empty.toml");
    std::fs::write(&config_path, "").unwrap();
    let record_dir = dir.join("records");
    std::fs::create_dir_all(&record_dir).unwrap();

    let mut child = tokio::process::Command::new(proxy_bin())
        .arg(config_path.to_str().unwrap())
        .args(["--monitor-listen-addr", "127.0.0.1:0"])
        .arg("--record-dir")
        .arg(record_dir.to_str().unwrap())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .unwrap();

    // Read the process's own stdout until it logs the bound monitor address.
    let stdout = child.stdout.take().unwrap();
    let mut lines = BufReader::new(stdout).lines();
    let addr = loop {
        let line = tokio::time::timeout(Duration::from_secs(30), lines.next_line())
            .await
            .expect("timed out waiting for the monitor address log")
            .expect("failed to read the process stdout")
            .unwrap_or_else(|| panic!("the process exited before logging the monitor address"));
        if let Some(rest) = line.split("listening addr: ").nth(1) {
            break rest.trim().to_string();
        }
    };

    let health = http_get(&addr, "/health").await;
    assert!(
        health.starts_with("HTTP/1.1 200"),
        "the monitor must serve /health: {health}"
    );
    let metrics = http_get(&addr, "/metrics").await;
    assert!(
        metrics.starts_with("HTTP/1.1 200"),
        "the monitor must serve /metrics: {metrics}"
    );
    let sessions = http_get(&addr, "/sessions").await;
    assert!(
        sessions.starts_with("HTTP/1.1 200"),
        "the monitor must serve /sessions: {sessions}"
    );
    // Both tables must be rendered, stream before udp: swapping the two
    // blocks in `sessions` would query each table with the other's SQL.
    let stream_at = sessions
        .find("Stream:")
        .unwrap_or_else(|| panic!("the monitor must render both session tables: {sessions}"));
    let udp_at = sessions
        .find("UDP:")
        .unwrap_or_else(|| panic!("the monitor must render both session tables: {sessions}"));
    assert!(
        stream_at < udp_at,
        "the stream table must be rendered before the udp table: {sessions}"
    );

    child.kill().await.ok();
    std::fs::remove_dir_all(&dir).ok();
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
        // An escape sequence is CSI: ESC '[' then parameter bytes then a
        // final byte in `@`..=`~`; skip through the final byte.
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

/// Spawn the binary on `config_path` with an ephemeral monitor listener and
/// return the child plus the monitor and access-server addresses it logged.
/// The access-server address is the one its listener actually bound, so both
/// ports come from the OS and neither can be taken by another process.
///
/// Both reads are bounded: a process that never logs its addresses fails
/// instead of hanging, and one that exits is named as such.
async fn spawn_and_learn_addrs(
    config_path: &std::path::Path,
) -> (tokio::process::Child, String, String) {
    let mut child = tokio::process::Command::new(proxy_bin())
        .arg(config_path.to_str().unwrap())
        .args(["--monitor-listen-addr", "127.0.0.1:0"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let stdout = child.stdout.take().unwrap();
    let mut lines = BufReader::new(stdout).lines();
    let mut monitor = None;
    let mut access = None;
    while monitor.is_none() || access.is_none() {
        let line = tokio::time::timeout(Duration::from_secs(30), lines.next_line())
            .await
            .expect("timed out waiting for the process to log its listener addresses")
            .expect("failed to read the process stdout")
            .unwrap_or_else(|| {
                panic!("the process exited before logging its monitor and access-server addresses")
            });
        let line = strip_ansi(&line);
        if let Some(rest) = line.split("listening addr: ").nth(1) {
            monitor = Some(rest.trim().to_string());
        }
        if let Some(rest) = line.split("Listening addr=").nth(1) {
            access = Some(rest.trim().to_string());
        }
    }
    (child, monitor.unwrap(), access.unwrap())
}

/// The two session blocks a `/sessions` response renders, each block's
/// non-blank lines with its header row first. This is the operator's view, so
/// the assertions below read exactly what an operator reads.
fn session_blocks(response: &str) -> (Vec<&str>, Vec<&str>) {
    let body = response
        .split("\r\n\r\n")
        .nth(1)
        .unwrap_or_else(|| panic!("the /sessions response must have a body: {response}"));
    let after_stream = body
        .split_once("Stream:")
        .unwrap_or_else(|| panic!("the /sessions body must render the stream block: {body}"))
        .1;
    let (stream, udp) = after_stream
        .split_once("UDP:")
        .unwrap_or_else(|| panic!("the /sessions body must render the udp block: {body}"));
    fn lines(block: &str) -> Vec<&str> {
        block.lines().filter(|l| !l.trim().is_empty()).collect()
    }
    (lines(stream), lines(udp))
}

/// A config path that does not exist makes the watcher root task fail, which
/// is fatal to the process. The run still binds and starts the monitoring
/// server first, so it exercises the monitor branch and the root-task
/// failure path and then exits normally (letting the coverage runtime flush).
#[tokio::test]
async fn a_missing_config_file_is_fatal_after_the_monitor_starts() {
    let dir = unique_temp_dir("missing");
    std::fs::create_dir_all(&dir).unwrap();
    let missing = dir.join("does-not-exist.toml");
    let record_dir = dir.join("records");
    std::fs::create_dir_all(&record_dir).unwrap();
    let output = tokio::process::Command::new(proxy_bin())
        .arg(missing.to_str().unwrap())
        .args(["--monitor-listen-addr", "127.0.0.1:0"])
        .arg("--record-dir")
        .arg(record_dir.to_str().unwrap())
        .output()
        .await
        .unwrap();
    assert_eq!(
        output.status.code(),
        Some(1),
        "a missing config must be fatal"
    );
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        text.contains("Monitoring HTTP server listening addr:"),
        "the monitor branch must run before the fatal config failure: {text}"
    );
    assert!(
        text.contains("config_watcher"),
        "the exit must name the failed root task: {text}"
    );
    // `--record-dir` installs both CSV loggers, which open their first epoch
    // file before the fatal config failure.
    assert!(
        record_dir.join("stream_record").join("0.csv").exists(),
        "the stream record logger must be installed"
    );
    assert!(
        record_dir.join("udp_record").join("0.csv").exists(),
        "the udp record logger must be installed"
    );
    std::fs::remove_dir_all(&dir).ok();
}

/// The CLI's short flags and documented aliases are part of the operator's
/// interface: `-m`/`--monitor` and `-r`/`--csv-log-path` must be accepted
/// exactly like the long names. A rejected flag is clap's usage error (exit
/// code 2), so a run that reaches the fatal missing-config exit (code 1)
/// after starting the monitor and installing both record loggers is what
/// proves the flag was accepted.
#[tokio::test]
async fn the_cli_short_flags_and_aliases_are_accepted() {
    for (monitor_flag, record_flag) in [("-m", "-r"), ("--monitor", "--csv-log-path")] {
        let dir = unique_temp_dir("cli-alias");
        std::fs::create_dir_all(&dir).unwrap();
        let missing = dir.join("does-not-exist.toml");
        let record_dir = dir.join("records");
        std::fs::create_dir_all(&record_dir).unwrap();
        let output = tokio::process::Command::new(proxy_bin())
            .arg(missing.to_str().unwrap())
            .arg(monitor_flag)
            .arg("127.0.0.1:0")
            .arg(record_flag)
            .arg(record_dir.to_str().unwrap())
            .output()
            .await
            .unwrap();
        let text = format!(
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(
            output.status.code(),
            Some(1),
            "`{monitor_flag}`/`{record_flag}` must be accepted (clap rejects an unknown \
             flag with exit code 2): {text}"
        );
        assert!(
            text.contains("Monitoring HTTP server listening addr:"),
            "`{monitor_flag}` must select the monitor listener: {text}"
        );
        assert!(
            record_dir.join("stream_record").join("0.csv").exists(),
            "`{record_flag}` must install the stream record logger"
        );
        assert!(
            record_dir.join("udp_record").join("0.csv").exists(),
            "`{record_flag}` must install the udp record logger"
        );
        std::fs::remove_dir_all(&dir).ok();
    }
}

/// A config file that exists but cannot be read as TOML fails the initial
/// serve preparation; `main` must return that error and exit normally. The
/// normal exit (not a kill) is what lets the coverage runtime flush, and it
/// exercises the no-monitor serve-context branch.
#[tokio::test(flavor = "multi_thread")]
async fn an_unreadable_config_exits_the_process_normally() {
    let dir = unique_temp_dir("bad-config");
    std::fs::create_dir_all(&dir).unwrap();
    let config_path = dir.join("bad.toml");
    std::fs::write(&config_path, "this is not = valid toml [[[").unwrap();

    let child = tokio::process::Command::new(proxy_bin())
        .arg(config_path.to_str().unwrap())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .unwrap();

    let output = tokio::time::timeout(Duration::from_secs(30), child.wait_with_output())
        .await
        .expect("the process must exit on an unreadable config, not hang")
        .unwrap();
    assert!(
        !output.status.success(),
        "an unreadable config must be fatal: {output:?}"
    );
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        text.contains("bad.toml"),
        "the failure must name the config file it could not read: {text}"
    );
    assert!(
        text.contains("Config"),
        "the failure must be classified as a config error: {text}"
    );
    std::fs::remove_dir_all(&dir).ok();
}

/// The monitor branch of `main` hands the runtime the stream session table
/// the `/sessions` view reads, so a stream session established through the
/// serve path is recorded and rendered with the session's own destination. A
/// runtime handed `None` instead serves the connection identically — the
/// accept, the upstream dial and the io copy all happen — and leaves no
/// record anywhere, which is what this pins.
///
/// The destination is a listener the test owns on an ephemeral port, and the
/// access server's own port is read from the address its listener logs. Both
/// ports are therefore chosen by the OS: nothing binds a fixed port and
/// nothing has to retry a taken one. The trigger is the accept on the
/// test-owned listener — the access server dials its destination before it
/// starts the recorded copy — so the only wait is a bounded poll for the row
/// that follows it, never a sleep for an expected latency.
#[tokio::test(flavor = "multi_thread")]
async fn a_stream_session_established_by_the_serve_path_is_recorded() {
    let responder = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("binding an ephemeral loopback listener must succeed");
    let responder_port = responder
        .local_addr()
        .expect("a bound listener has a local address")
        .port();

    let dir = unique_temp_dir("stream-session");
    std::fs::create_dir_all(&dir).unwrap();
    let config_path = dir.join("session.toml");
    // An empty chain dials the destination directly, so the session exists as
    // soon as the responder accepts the connection.
    std::fs::write(
        &config_path,
        format!(
            r#"
[access_server.stream.conn_selector]
"default" = {{ chains = [] }}

[[access_server.tcp_server]]
listen_addr = "127.0.0.1:0"
destination = "tcp://127.0.0.1:{responder_port}"
conn_selector = "default"
"#
        ),
    )
    .unwrap();

    let (mut child, monitor, access) = spawn_and_learn_addrs(&config_path).await;

    let client = TcpStream::connect(&access)
        .await
        .expect("the access-server listener must accept a connection");
    let (upstream, _peer) = tokio::time::timeout(Duration::from_secs(30), responder.accept())
        .await
        .expect("the access server must dial the configured destination")
        .expect("the responder must accept the access server's dial");

    // The recorded row follows the dial above on loopback; this budget bounds
    // how long a runtime that never records it can look like one that has not
    // got there yet.
    const ROW_BUDGET: Duration = Duration::from_secs(15);
    let deadline = tokio::time::Instant::now() + ROW_BUDGET;
    let sessions = loop {
        let response = http_get(&monitor, "/sessions").await;
        if session_blocks(&response).0.len() >= 2 {
            break response;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the serve path accepted the connection and dialed the destination, but no stream \
             session was recorded in {ROW_BUDGET:?}. The metrics of the run: {}",
            http_get(&monitor, "/metrics").await,
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    };

    let (stream_rows, _) = session_blocks(&sessions);
    assert_eq!(
        stream_rows.len(),
        2,
        "the stream session view must be its header plus exactly the one session this test \
         established, and nothing else: {sessions}"
    );
    let port = responder_port.to_string();
    assert!(
        stream_rows[1].split_whitespace().any(|token| token == port),
        "the recorded row must be the session this test established, whose destination is \
         127.0.0.1:{port}: {}",
        stream_rows[1]
    );

    drop(client);
    drop(upstream);
    child.kill().await.ok();
    std::fs::remove_dir_all(&dir).ok();
}

/// The same wiring for the udp session table: a udp session established
/// through the serve path is recorded with the flow's own destination. The
/// stream and udp tables are separate columns of the same serve context, so
/// neither may be handed the other's table, dropped, or left unset.
#[tokio::test(flavor = "multi_thread")]
async fn a_udp_session_established_by_the_serve_path_is_recorded() {
    let responder = UdpSocket::bind("127.0.0.1:0")
        .await
        .expect("binding an ephemeral loopback socket must succeed");
    let responder_port = responder
        .local_addr()
        .expect("a bound socket has a local address")
        .port();

    let dir = unique_temp_dir("udp-session");
    std::fs::create_dir_all(&dir).unwrap();
    let config_path = dir.join("session.toml");
    std::fs::write(
        &config_path,
        format!(
            r#"
[access_server.udp.conn_selector]
"default" = {{ chains = [] }}

[[access_server.udp_server]]
listen_addr = "127.0.0.1:0"
destination = "127.0.0.1:{responder_port}"
conn_selector = "default"
"#
        ),
    )
    .unwrap();

    let (mut child, monitor, access) = spawn_and_learn_addrs(&config_path).await;

    let payload = b"session-wiring-probe";
    let client = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    client
        .send_to(payload, &access)
        .await
        .expect("the access-server socket must accept a datagram");
    let mut buf = [0u8; 64];
    let (n, _from) = tokio::time::timeout(Duration::from_secs(30), responder.recv_from(&mut buf))
        .await
        .expect("the access server must forward the datagram to the configured destination")
        .unwrap();
    assert_eq!(&buf[..n], payload, "the datagram must arrive unaltered");

    const ROW_BUDGET: Duration = Duration::from_secs(15);
    let deadline = tokio::time::Instant::now() + ROW_BUDGET;
    let sessions = loop {
        let response = http_get(&monitor, "/sessions").await;
        if session_blocks(&response).1.len() >= 2 {
            break response;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the serve path forwarded the datagram to the destination, but no udp session was \
             recorded in {ROW_BUDGET:?}. The metrics of the run: {}",
            http_get(&monitor, "/metrics").await,
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    };

    let (_, udp_rows) = session_blocks(&sessions);
    assert_eq!(
        udp_rows.len(),
        2,
        "the udp session view must be its header plus exactly the one session this test \
         established, and nothing else: {sessions}"
    );
    let port = responder_port.to_string();
    assert!(
        udp_rows[1].split_whitespace().any(|token| token == port),
        "the recorded row must be the flow this test established, whose destination is \
         127.0.0.1:{port}: {}",
        udp_rows[1]
    );

    drop(client);
    child.kill().await.ok();
    std::fs::remove_dir_all(&dir).ok();
}
