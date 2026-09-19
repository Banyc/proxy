//! End-to-end exercise of the HTTP access server over real TCP sockets: the
//! production handler is bound through its real `TcpServer` and driven by a
//! raw HTTP/1.1 client, asserting byte-exact CONNECT tunnelling, byte-exact
//! non-CONNECT proxying, the blocked-ruleset rejection, and the failure of a
//! refused upstream.
//!
//! All listeners bind `127.0.0.1:0`; the test reads the actual port, so it
//! never races a fixed port. Every background task lives in the test's
//! `JoinSet`, which aborts the lot when the test returns.

use std::{collections::HashMap, sync::Arc, time::Duration};

use ae::anti_replay::ReplayValidator;
use common::{
    addr::InternetAddr,
    anti_replay::{VALIDATOR_CAPACITY, VALIDATOR_TIME_FRAME},
    connect::{ConnectorConfig, ConnectorResetSignal, connector_config_cell},
    lifecycle::retention::{RetentionActor, RetentionActorSender},
    loading::{self, Serve},
    notify::Notify,
    proxy_runtime::{
        addr::RouteAddr,
        client::stream::StreamTracer,
        conn_handler::{SpeedLimit, stream::StreamProxyConnHandler},
        connect::udp::UdpConnector,
        context::StreamRuntime,
    },
    route::{
        HopConfig, ProbeFutures, ProbeRtt, RouteAction, RouteSelector, RouteTable, RouteTableEntry,
        WeightedRouteChain,
    },
    session::SessionSpawner,
    stream_runtime::pool::StreamConnPool,
};
use protocol::stream_proto::{
    addr::ConcreteStreamType,
    connect::build_concrete_stream_connector_table,
    streams::{
        http_tunnel::HttpAccessConnHandler,
        tcp::{listener::TcpServer, proxy_server::build_tcp_proxy_server},
    },
};
use swap::Swap;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    task::JoinSet,
};

/// Every background task a test owns; dropping it aborts them all.
type Tasks = JoinSet<()>;

fn spawn_process_actors(tasks: &mut Tasks) -> (SessionSpawner, RetentionActorSender) {
    let (session_spawner, mut session_rx) = SessionSpawner::channel();
    tasks.spawn(async move {
        let mut sessions = tokio::task::JoinSet::new();
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

fn stream_context(tasks: &mut Tasks) -> StreamRuntime {
    let (session_spawner, retention) = spawn_process_actors(tasks);
    let mut connector_drivers = tokio::task::JoinSet::new();
    let connector_config = connector_config_cell(ConnectorConfig::default()).0;
    let udp_connector = UdpConnector::new(connector_config.clone());
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
    StreamRuntime {
        session_table: None,
        pool: Swap::new(StreamConnPool::empty()),
        connector_table,
        replay_validator: Arc::new(ReplayValidator::new(
            VALIDATOR_TIME_FRAME,
            VALIDATOR_CAPACITY,
        )),
        session_spawner,
        retention,
    }
}

fn catch_all_matcher() -> common::matcher::Matcher {
    serde_json::from_str("{}").unwrap()
}

fn direct_route_table() -> RouteTable {
    RouteTable::new(
        vec![RouteTableEntry::new(
            None,
            catch_all_matcher(),
            RouteAction::Direct,
        )],
        Arc::new(HashMap::new()),
    )
}

fn blocking_route_table() -> RouteTable {
    RouteTable::new(
        vec![RouteTableEntry::new(
            None,
            catch_all_matcher(),
            RouteAction::Block,
        )],
        Arc::new(HashMap::new()),
    )
}

/// Spawn the production HTTP access handler behind its real `TcpServer` and
/// return the bound address.
async fn spawn_http_access(tasks: &mut Tasks, route_table: RouteTable) -> std::net::SocketAddr {
    let stream_context = stream_context(tasks);
    let listen_addr: Arc<str> = Arc::from("127.0.0.1:0");
    let handler = HttpAccessConnHandler::new(
        route_table,
        f64::INFINITY,
        stream_context.clone(),
        Arc::clone(&listen_addr),
    );
    let listener = TcpListener::bind(listen_addr.as_ref()).await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = TcpServer::new(listener, handler, stream_context.session_spawner.clone());
    let (set_conn_handler_tx, set_conn_handler_rx) = loading::replace_conn_handler_channel();
    tasks.spawn(async move {
        let _set_conn_handler_tx = set_conn_handler_tx;
        server.serve(set_conn_handler_rx).await.unwrap();
    });
    addr
}

/// A TCP origin that reads exactly `req` then writes `resp`, `accepts` times.
async fn spawn_tcp_greet(tasks: &mut Tasks, req: &[u8], resp: &[u8], accepts: usize) -> RouteAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let req = req.to_vec();
    let resp = resp.to_vec();
    tasks.spawn(async move {
        let mut handlers = tokio::task::JoinSet::new();
        let mut accepted = 0;
        while accepted < accepts {
            tokio::select! {
                accepted_conn = listener.accept() => {
                    let (mut stream, _) = accepted_conn.unwrap();
                    accepted += 1;
                    let req = req.clone();
                    let resp = resp.clone();
                    handlers.spawn(async move {
                        let mut buf = vec![0; req.len()];
                        stream.read_exact(&mut buf).await.unwrap();
                        assert_eq!(buf, req);
                        stream.write_all(&resp).await.unwrap();
                    });
                }
                Some(result) = handlers.join_next() => { result.unwrap(); }
            }
        }
        while let Some(result) = handlers.join_next().await {
            result.unwrap();
        }
    });
    RouteAddr {
        address: addr.into(),
        protocol: ConcreteStreamType::Tcp.to_string().into(),
    }
}

/// A minimal HTTP/1.1 origin: reads one request head, asserts it starts with
/// `expected_method`, and answers with `body`.
async fn spawn_http_origin(
    tasks: &mut Tasks,
    expected_method: &str,
    body: &[u8],
    accepts: usize,
) -> std::net::SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let expected_method = expected_method.to_owned();
    let body = body.to_vec();
    tasks.spawn(async move {
        let mut handlers = tokio::task::JoinSet::new();
        let mut accepted = 0;
        while accepted < accepts {
            tokio::select! {
                accepted_conn = listener.accept() => {
                    let (mut stream, _) = accepted_conn.unwrap();
                    accepted += 1;
                    let expected_method = expected_method.clone();
                    let body = body.clone();
                    handlers.spawn(async move {
                        let mut head = Vec::new();
                        let mut byte = [0u8; 1];
                        loop {
                            let n = stream.read(&mut byte).await.unwrap();
                            if n == 0 {
                                return;
                            }
                            head.push(byte[0]);
                            if head.ends_with(b"\r\n\r\n") {
                                break;
                            }
                        }
                        let head = String::from_utf8(head).unwrap();
                        assert!(
                            head.starts_with(&expected_method),
                            "origin got an unexpected request: {head:?}"
                        );
                        let response = format!(
                            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                            body.len()
                        );
                        stream.write_all(response.as_bytes()).await.unwrap();
                        stream.write_all(&body).await.unwrap();
                    });
                }
                Some(result) = handlers.join_next() => { result.unwrap(); }
            }
        }
        while let Some(result) = handlers.join_next().await {
            result.unwrap();
        }
    });
    addr
}

/// Read exactly the HTTP response head (through the blank line).
async fn read_http_head(stream: &mut TcpStream) -> String {
    let mut head = Vec::new();
    let mut byte = [0u8; 1];
    loop {
        let n = tokio::time::timeout(Duration::from_secs(10), stream.read(&mut byte))
            .await
            .expect("timed out reading the response head")
            .unwrap();
        if n == 0 {
            break;
        }
        head.push(byte[0]);
        if head.ends_with(b"\r\n\r\n") {
            break;
        }
    }
    String::from_utf8(head).unwrap()
}

async fn read_exact_response(stream: &mut TcpStream, resp_msg: &[u8]) {
    let mut buf = vec![0; resp_msg.len()];
    tokio::time::timeout(Duration::from_secs(10), stream.read_exact(&mut buf))
        .await
        .expect("timed out waiting for the relayed response")
        .unwrap();
    assert_eq!(buf, resp_msg);
}

/// Send a CONNECT request for `destination` and return the response head.
async fn http_connect(stream: &mut TcpStream, destination: &InternetAddr) -> String {
    let request = format!("CONNECT {destination} HTTP/1.1\r\nHost: {destination}\r\n\r\n");
    stream.write_all(request.as_bytes()).await.unwrap();
    read_http_head(stream).await
}

#[tokio::test(flavor = "multi_thread")]
async fn connect_direct_relays_bytes_byte_exact() {
    let mut tasks = Tasks::new();
    let server_addr = spawn_http_access(&mut tasks, direct_route_table()).await;
    let req_msg = b"hello through the http connect tunnel";
    let resp_msg = b"goodbye through the http connect tunnel";
    let greet = spawn_tcp_greet(&mut tasks, req_msg, resp_msg, 1).await;

    let mut stream = TcpStream::connect(server_addr).await.unwrap();
    let head = http_connect(&mut stream, &greet.address).await;
    assert!(
        head.starts_with("HTTP/1.1 200"),
        "a CONNECT to a direct destination must be accepted: {head:?}"
    );
    stream.write_all(req_msg).await.unwrap();
    read_exact_response(&mut stream, resp_msg).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn connect_to_a_blocked_destination_is_refused() {
    let mut tasks = Tasks::new();
    let server_addr = spawn_http_access(&mut tasks, blocking_route_table()).await;
    let destination: InternetAddr = "127.0.0.1:9".parse().unwrap();

    let mut stream = TcpStream::connect(server_addr).await.unwrap();
    let head = http_connect(&mut stream, &destination).await;
    assert!(
        head.starts_with("HTTP/1.1 503"),
        "a blocked CONNECT must answer 503, not connect: {head:?}"
    );
    assert!(
        head.to_ascii_lowercase().contains("content-length: 7"),
        "the rejection must carry the Blocked body length: {head:?}"
    );
    let mut body = [0u8; 7];
    tokio::time::timeout(Duration::from_secs(10), stream.read_exact(&mut body))
        .await
        .expect("timed out reading the rejection body")
        .unwrap();
    assert_eq!(&body, b"Blocked");
}

#[tokio::test(flavor = "multi_thread")]
async fn connect_to_a_refused_upstream_closes_the_tunnel() {
    let mut tasks = Tasks::new();
    let server_addr = spawn_http_access(&mut tasks, direct_route_table()).await;
    // Port 1 on loopback has no listener, so the upstream connect is refused.
    let destination: InternetAddr = "127.0.0.1:1".parse().unwrap();

    let mut stream = TcpStream::connect(server_addr).await.unwrap();
    let head = http_connect(&mut stream, &destination).await;
    assert!(
        head.starts_with("HTTP/1.1 200"),
        "the tunnel is advertised before the upstream is established: {head:?}"
    );
    // The upstream connect fails after the 200; the relay must end the
    // connection rather than hang.
    let mut buf = [0u8; 16];
    let outcome = tokio::time::timeout(Duration::from_secs(10), stream.read(&mut buf)).await;
    match outcome {
        Ok(Ok(0)) | Ok(Err(_)) => {}
        Ok(Ok(n)) => panic!("a refused upstream must not relay bytes, got {n} bytes"),
        Err(_) => panic!("a refused upstream must close the tunnel, not hang"),
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn non_connect_get_proxies_to_the_origin_byte_exact() {
    let mut tasks = Tasks::new();
    let server_addr = spawn_http_access(&mut tasks, direct_route_table()).await;
    let body = b"proxied origin response body";
    let origin = spawn_http_origin(&mut tasks, "GET ", body, 1).await;

    let mut stream = TcpStream::connect(server_addr).await.unwrap();
    let request = format!(
        "GET http://127.0.0.1:{}/some/path HTTP/1.1\r\nHost: 127.0.0.1:{}\r\n\r\n",
        origin.port(),
        origin.port()
    );
    stream.write_all(request.as_bytes()).await.unwrap();
    let head = read_http_head(&mut stream).await;
    assert!(
        head.starts_with("HTTP/1.1 200"),
        "an absolute-form GET must be proxied: {head:?}"
    );
    let mut got = vec![0u8; body.len()];
    tokio::time::timeout(Duration::from_secs(10), stream.read_exact(&mut got))
        .await
        .expect("timed out reading the proxied body")
        .unwrap();
    assert_eq!(&got, body);
}

#[tokio::test(flavor = "multi_thread")]
async fn non_connect_get_to_a_blocked_destination_is_refused() {
    let mut tasks = Tasks::new();
    let server_addr = spawn_http_access(&mut tasks, blocking_route_table()).await;

    let mut stream = TcpStream::connect(server_addr).await.unwrap();
    let request = "GET http://127.0.0.1:9/ HTTP/1.1\r\nHost: 127.0.0.1:9\r\n\r\n";
    stream.write_all(request.as_bytes()).await.unwrap();
    let head = read_http_head(&mut stream).await;
    assert!(
        head.starts_with("HTTP/1.1 503"),
        "a blocked GET must answer 503: {head:?}"
    );
    let mut body = [0u8; 7];
    tokio::time::timeout(Duration::from_secs(10), stream.read_exact(&mut body))
        .await
        .expect("timed out reading the rejection body")
        .unwrap();
    assert_eq!(&body, b"Blocked");
}

/// Spawn a loopback-refusing TCP proxy hop (so a test can tell the configured
/// chain was used) and return its chain config.
async fn spawn_guarded_tcp_proxy(tasks: &mut Tasks) -> HopConfig {
    let stream_context = stream_context(tasks);
    let crypto = tokio_chacha20::config::Config::new([0x42; 32].into());
    let proxy = StreamProxyConnHandler::new(
        crypto.clone(),
        None,
        stream_context.clone(),
        Arc::from("127.0.0.1:0"),
        false,
        SpeedLimit::UNLIMITED,
    );
    let server =
        build_tcp_proxy_server("127.0.0.1:0", proxy, stream_context.session_spawner.clone())
            .await
            .unwrap();
    let addr = server.listener().local_addr().unwrap();
    let (set_conn_handler_tx, set_conn_handler_rx) = loading::replace_conn_handler_channel();
    tasks.spawn(async move {
        let _set_conn_handler_tx = set_conn_handler_tx;
        server.serve(set_conn_handler_rx).await.unwrap();
    });
    HopConfig {
        name: None,
        address: RouteAddr {
            address: addr.into(),
            protocol: ConcreteStreamType::Tcp.to_string().into(),
        },
        header_crypto: crypto,
        payload_crypto: None,
    }
}

fn chained_route_table(tasks: &mut Tasks, hop: HopConfig) -> RouteTable {
    let stream_context = stream_context(tasks);
    let mut probes = ProbeFutures::new();
    let tracer: Arc<dyn ProbeRtt + Send + Sync> = Arc::new(StreamTracer::new(stream_context));
    let selector = RouteSelector::new(
        vec![WeightedRouteChain {
            weight: 1,
            chain: Arc::from(vec![hop]),
        }],
        Some(tracer),
        None,
        None,
        tokio_util::sync::CancellationToken::new(),
        &mut probes,
    )
    .unwrap();
    assert!(
        !probes.is_empty(),
        "a non-empty chain selector must collect a probe future"
    );
    RouteTable::new(
        vec![RouteTableEntry::new(
            None,
            catch_all_matcher(),
            RouteAction::RouteSelector(Arc::new(selector)),
        )],
        Arc::new(HashMap::new()),
    )
}

#[tokio::test(flavor = "multi_thread")]
async fn connect_routes_through_a_configured_proxy_chain() {
    let mut tasks = Tasks::new();
    let hop = spawn_guarded_tcp_proxy(&mut tasks).await;
    let route_table = chained_route_table(&mut tasks, hop);
    let server_addr = spawn_http_access(&mut tasks, route_table).await;
    let req_msg = b"hello through the http proxy chain";
    let greet = spawn_tcp_greet(&mut tasks, req_msg, b"never relayed", 1).await;

    let mut stream = TcpStream::connect(server_addr).await.unwrap();
    let head = http_connect(&mut stream, &greet.address).await;
    assert!(
        head.starts_with("HTTP/1.1 200"),
        "the access server establishes the chain before relaying: {head:?}"
    );
    // The guarded hop refuses the loopback destination; if the access server
    // had ignored the configured chain and connected directly, the loopback
    // origin would have answered.
    let _ = stream.write_all(req_msg).await;
    let mut buf = [0u8; 64];
    let outcome = tokio::time::timeout(Duration::from_secs(5), stream.read(&mut buf)).await;
    match outcome {
        Ok(Ok(0)) | Ok(Err(_)) => {}
        Ok(Ok(n)) => panic!(
            "the configured chain was bypassed: read {n} bytes: {:?}",
            &buf[..n]
        ),
        Err(_) => panic!("the configured chain was bypassed: the relay did not terminate"),
    }
}

/// A WebSocket-style origin: reads the request head, asserts the upgrade
/// request was forwarded intact, answers `101 Switching Protocols`, then
/// echoes one `ping`/`pong` exchange over the upgraded connection.
async fn spawn_ws_echo_origin(tasks: &mut Tasks) -> std::net::SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tasks.spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let mut head = Vec::new();
        let mut byte = [0u8; 1];
        loop {
            let n = stream.read(&mut byte).await.unwrap();
            if n == 0 {
                return;
            }
            head.push(byte[0]);
            if head.ends_with(b"\r\n\r\n") {
                break;
            }
        }
        let head = String::from_utf8(head).unwrap();
        assert!(head.starts_with("GET "), "origin got {head:?}");
        assert!(
            head.to_ascii_lowercase().contains("upgrade: websocket"),
            "the upgrade header must be forwarded intact: {head:?}"
        );
        stream
            .write_all(
                b"HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n\r\n",
            )
            .await
            .unwrap();
        let mut buf = [0u8; 4];
        stream.read_exact(&mut buf).await.unwrap();
        assert_eq!(&buf, b"ping");
        stream.write_all(b"pong").await.unwrap();
    });
    addr
}

/// When the origin answers `101 Switching Protocols`, the access server must
/// hand the upgraded connection to both peers and tunnel bytes byte-exactly,
/// rather than returning the 101 and closing.
#[tokio::test(flavor = "multi_thread")]
async fn an_upgrade_response_is_tunnelled_after_the_101() {
    let mut tasks = Tasks::new();
    let server_addr = spawn_http_access(&mut tasks, direct_route_table()).await;
    let origin = spawn_ws_echo_origin(&mut tasks).await;

    let mut stream = TcpStream::connect(server_addr).await.unwrap();
    let request = format!(
        "GET http://{origin}/ws HTTP/1.1\r\nHost: {origin}\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n\r\n"
    );
    stream.write_all(request.as_bytes()).await.unwrap();
    let head = read_http_head(&mut stream).await;
    assert!(
        head.starts_with("HTTP/1.1 101"),
        "the origin's 101 must be relayed: {head:?}"
    );
    assert!(
        head.to_ascii_lowercase().contains("upgrade: websocket"),
        "the 101 must keep its upgrade header: {head:?}"
    );
    stream.write_all(b"ping").await.unwrap();
    let mut buf = [0u8; 4];
    tokio::time::timeout(Duration::from_secs(10), stream.read_exact(&mut buf))
        .await
        .expect("timed out reading the tunnelled echo")
        .unwrap();
    assert_eq!(&buf, b"pong");
}
