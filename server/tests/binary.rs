//! Exercise the `proxy` binary as a process: the CLI surface (`--help`, the
//! no-config error), and the running server with its monitoring HTTP server
//! and CSV record directory. The binary is located through cargo's
//! `CARGO_BIN_EXE_proxy`, so the test drives the real production entry point.
//!
//! The monitor listener binds `127.0.0.1:0`; the test reads the actual port
//! from the process's own startup log, so it never races a fixed port. Every
//! spawned process is `kill_on_drop`, so a failed assertion does not leak it.

use std::{process::Stdio, time::Duration};

use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    net::TcpStream,
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
