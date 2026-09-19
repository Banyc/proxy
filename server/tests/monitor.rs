//! Exercise the monitoring HTTP router in process: the axum router assembled
//! from `monitor_router()` is served on an ephemeral port and each route is
//! requested over a real socket. This is the same router `main` installs, so
//! the handlers and the default session queries are driven directly.
//!
//! The recorder is a process-global, so this test lives in its own integration
//! test binary (the only place `monitor_router` is called in this process).

use std::time::Duration;

use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
};

async fn http_get(addr: std::net::SocketAddr, path: &str) -> String {
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
async fn the_monitor_router_serves_health_metrics_and_both_session_tables() {
    let (_session_tables, router) = server::monitor::monitor_router();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let mut tasks: tokio::task::JoinSet<()> = tokio::task::JoinSet::new();
    tasks.spawn(async move {
        let _ = axum::serve(listener, router.into_make_service()).await;
    });

    let health = http_get(addr, "/health").await;
    assert!(
        health.starts_with("HTTP/1.1 200"),
        "the monitor must serve /health: {health}"
    );

    let metrics = http_get(addr, "/metrics").await;
    assert!(
        metrics.starts_with("HTTP/1.1 200"),
        "the monitor must serve /metrics: {metrics}"
    );

    let sessions = http_get(addr, "/sessions").await;
    assert!(
        sessions.starts_with("HTTP/1.1 200"),
        "the monitor must serve /sessions: {sessions}"
    );
    assert!(
        sessions.contains("Stream:") && sessions.contains("UDP:"),
        "the session view must render both tables in order: {sessions}"
    );

    // The alias query parameters select the caller's SQL rather than the
    // defaults; the response still renders both tables.
    let aliased = http_get(
        addr,
        "/sessions?stream_sql=sort%20start_ms&udp_sql=sort%20start_ms",
    )
    .await;
    assert!(
        aliased.starts_with("HTTP/1.1 200") && aliased.contains("Stream:"),
        "the aliased query must be accepted: {aliased}"
    );

    tasks.shutdown().await;
}
