//! End-to-end exercise of the SOCKS5 access server over real TCP/UDP
//! sockets: the production handler is bound through its real server type and
//! driven by a wire-level SOCKS5 client, asserting byte-exact relay and the
//! reply/error classification of each command arm.
//!
//! All listeners bind `127.0.0.1:0`; the test reads the actual port, so it
//! never races a fixed port. Every background task lives in the test's
//! `JoinSet`, which aborts the lot when the test returns.

use std::{collections::HashMap, sync::Arc, time::Duration};

use ae::anti_replay::{ReplayValidator, TimeValidator};
use common::{
    addr::InternetAddr,
    anti_replay::{VALIDATOR_CAPACITY, VALIDATOR_TIME_FRAME, VALIDATOR_UDP_HDR_TTL},
    connect::{ConnectorConfig, ConnectorResetSignal, connector_config_cell},
    lifecycle::retention::{RetentionActor, RetentionActorSender},
    loading::{self, Serve},
    notify::Notify,
    proxy_runtime::{
        addr::RouteAddr,
        client::stream::StreamTracer,
        conn_handler::{SpeedLimit, stream::StreamProxyConnHandler},
        connect::udp::UdpConnector,
        context::{StreamRuntime, UdpRuntime},
    },
    route::{
        HopConfig, ProbeFutures, ProbeRtt, RouteAction, RouteSelector, RouteTable, RouteTableEntry,
        WeightedRouteChain,
    },
    session::SessionSpawner,
    stream_runtime::pool::StreamConnPool,
};
use protocol::{
    socks5::{
        messages::{
            Command, MethodIdentifier, NegotiationRequest, NegotiationResponse, RelayRequest,
            RelayResponse, Reply, UdpRequestHeader,
        },
        server::{tcp::Socks5ServerTcpAccessConnHandler, udp::Socks5ServerUdpAccessConnHandler},
    },
    stream_proto::{
        addr::ConcreteStreamType,
        connect::build_concrete_stream_connector_table,
        streams::tcp::{listener::TcpServer, proxy_server::build_tcp_proxy_server},
    },
};
use swap::Swap;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream, UdpSocket},
    task::JoinSet,
};

/// Every background task a test owns; dropping it aborts them all.
type Tasks = JoinSet<()>;

/// The process actors every test needs: the session actor and the retention
/// actor.
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

/// A `StreamRuntime` whose connector table has every concrete stream dialer
/// registered, with the connector-driver reaper owned by the test's tasks.
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

fn udp_context(tasks: &mut Tasks) -> UdpRuntime {
    let (session_spawner, retention) = spawn_process_actors(tasks);
    UdpRuntime {
        session_table: None,
        connector: Arc::new(UdpConnector::new(
            connector_config_cell(ConnectorConfig::default()).0,
        )),
        time_validator: Arc::new(TimeValidator::new(
            VALIDATOR_TIME_FRAME + VALIDATOR_UDP_HDR_TTL,
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

/// Spawn the production SOCKS5 TCP access handler behind its real `TcpServer`
/// and return the bound address.
async fn spawn_socks5_tcp(
    tasks: &mut Tasks,
    route_table: RouteTable,
    udp_listen_addr: Option<InternetAddr>,
    users: HashMap<Arc<[u8]>, Arc<[u8]>>,
) -> std::net::SocketAddr {
    let stream_context = stream_context(tasks);
    let listen_addr: Arc<str> = Arc::from("127.0.0.1:0");
    let handler = Socks5ServerTcpAccessConnHandler::new(
        route_table,
        f64::INFINITY,
        udp_listen_addr,
        users,
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

/// A TCP echo-style origin: reads exactly `req` then writes `resp`, `accepts`
/// times.
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
        // All accepts are done; drain the in-flight handlers so the spawned
        // greet task does not abort them by dropping its `JoinSet`.
        while let Some(result) = handlers.join_next().await {
            result.unwrap();
        }
    });
    RouteAddr {
        address: addr.into(),
        protocol: ConcreteStreamType::Tcp.to_string().into(),
    }
}

async fn socks5_negotiate_no_auth(stream: &mut TcpStream) {
    NegotiationRequest {
        methods: vec![MethodIdentifier::NoAuth],
    }
    .encode(stream)
    .await
    .unwrap();
    let response = NegotiationResponse::decode(stream).await.unwrap();
    assert_eq!(
        response.method,
        Some(MethodIdentifier::NoAuth),
        "an empty user set must select the offered no-auth method"
    );
}

async fn socks5_connect(stream: &mut TcpStream, destination: InternetAddr) -> RelayResponse {
    RelayRequest {
        command: Command::Connect,
        destination,
    }
    .encode(stream)
    .await
    .unwrap();
    RelayResponse::decode(stream).await.unwrap()
}

async fn read_exact_response(stream: &mut TcpStream, resp_msg: &[u8]) {
    let mut buf = vec![0; resp_msg.len()];
    tokio::time::timeout(Duration::from_secs(10), stream.read_exact(&mut buf))
        .await
        .expect("timed out waiting for the relay response")
        .unwrap();
    assert_eq!(buf, resp_msg);
}

#[tokio::test(flavor = "multi_thread")]
async fn socks5_tcp_direct_relays_bytes_byte_exact() {
    let mut tasks = Tasks::new();
    let server_addr =
        spawn_socks5_tcp(&mut tasks, direct_route_table(), None, HashMap::new()).await;
    let req_msg = b"hello through the socks5 access server";
    let resp_msg = b"goodbye through the socks5 access server";
    let greet = spawn_tcp_greet(&mut tasks, req_msg, resp_msg, 1).await;

    let mut stream = TcpStream::connect(server_addr).await.unwrap();
    socks5_negotiate_no_auth(&mut stream).await;
    let response = socks5_connect(&mut stream, greet.address.clone()).await;
    assert_eq!(
        response.reply,
        Reply::Succeeded,
        "a direct destination must be accepted"
    );
    assert_eq!(
        response.bind,
        InternetAddr::from(server_addr),
        "the reply must advertise the access server's own socket as the bind address"
    );
    stream.write_all(req_msg).await.unwrap();
    read_exact_response(&mut stream, resp_msg).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn socks5_tcp_bind_command_is_refused() {
    let mut tasks = Tasks::new();
    let server_addr =
        spawn_socks5_tcp(&mut tasks, direct_route_table(), None, HashMap::new()).await;
    let destination: InternetAddr = "127.0.0.1:9".parse().unwrap();

    let mut stream = TcpStream::connect(server_addr).await.unwrap();
    socks5_negotiate_no_auth(&mut stream).await;
    RelayRequest {
        command: Command::Bind,
        destination,
    }
    .encode(&mut stream)
    .await
    .unwrap();
    let response = RelayResponse::decode(&mut stream).await.unwrap();
    assert_eq!(
        response.reply,
        Reply::CommandNotSupported,
        "BIND is not implemented and must be refused"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn socks5_tcp_udp_associate_without_a_udp_server_is_refused() {
    let mut tasks = Tasks::new();
    let server_addr =
        spawn_socks5_tcp(&mut tasks, direct_route_table(), None, HashMap::new()).await;
    let destination: InternetAddr = "127.0.0.1:9".parse().unwrap();

    let mut stream = TcpStream::connect(server_addr).await.unwrap();
    socks5_negotiate_no_auth(&mut stream).await;
    RelayRequest {
        command: Command::UdpAssociate,
        destination,
    }
    .encode(&mut stream)
    .await
    .unwrap();
    let response = RelayResponse::decode(&mut stream).await.unwrap();
    assert_eq!(
        response.reply,
        Reply::CommandNotSupported,
        "UDP ASSOCIATE must be refused when no UDP server is configured"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn socks5_tcp_udp_associate_advertises_the_configured_udp_listener() {
    let mut tasks = Tasks::new();
    let udp = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let udp_addr = udp.local_addr().unwrap();
    let server_addr = spawn_socks5_tcp(
        &mut tasks,
        direct_route_table(),
        Some(InternetAddr::from(udp_addr)),
        HashMap::new(),
    )
    .await;
    let destination: InternetAddr = "127.0.0.1:9".parse().unwrap();

    let mut stream = TcpStream::connect(server_addr).await.unwrap();
    socks5_negotiate_no_auth(&mut stream).await;
    RelayRequest {
        command: Command::UdpAssociate,
        destination,
    }
    .encode(&mut stream)
    .await
    .unwrap();
    let response = RelayResponse::decode(&mut stream).await.unwrap();
    assert_eq!(response.reply, Reply::Succeeded);
    assert_eq!(
        response.bind,
        InternetAddr::from(udp_addr),
        "the reply must carry the configured UDP listener address"
    );
    // The association ends when the client closes the control connection.
    let _ = stream.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn socks5_tcp_blocked_destination_is_refused() {
    let mut tasks = Tasks::new();
    let server_addr =
        spawn_socks5_tcp(&mut tasks, blocking_route_table(), None, HashMap::new()).await;
    let destination: InternetAddr = "127.0.0.1:9".parse().unwrap();

    let mut stream = TcpStream::connect(server_addr).await.unwrap();
    socks5_negotiate_no_auth(&mut stream).await;
    let response = socks5_connect(&mut stream, destination).await;
    assert_eq!(
        response.reply,
        Reply::ConnectionNotAllowedByRuleset,
        "a blocked destination must be refused by the ruleset, not connected"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn socks5_tcp_routes_through_a_configured_proxy_chain() {
    let mut tasks = Tasks::new();
    let stream_context = stream_context(&mut tasks);
    let hop = spawn_guarded_tcp_proxy(&mut tasks).await;
    let mut probes = ProbeFutures::new();
    let tracer: Arc<dyn ProbeRtt + Send + Sync> =
        Arc::new(StreamTracer::new(stream_context.clone()));
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
    let route_table = RouteTable::new(
        vec![RouteTableEntry::new(
            None,
            catch_all_matcher(),
            RouteAction::RouteSelector(Arc::new(selector)),
        )],
        Arc::new(HashMap::new()),
    );
    let server_addr = spawn_socks5_tcp(&mut tasks, route_table, None, HashMap::new()).await;
    let req_msg = b"hello through the socks5 proxy chain";
    let resp_msg = b"goodbye through the socks5 proxy chain";
    let greet = spawn_tcp_greet(&mut tasks, req_msg, resp_msg, 1).await;

    let mut stream = TcpStream::connect(server_addr).await.unwrap();
    socks5_negotiate_no_auth(&mut stream).await;
    let response = socks5_connect(&mut stream, greet.address.clone()).await;
    assert_eq!(
        response.reply,
        Reply::Succeeded,
        "the access server connected to the chain's first hop"
    );
    // The guarded hop refuses the loopback destination and drops the relay.
    // If the access server had ignored the configured chain and connected
    // directly, the loopback origin would have answered.
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

#[tokio::test(flavor = "multi_thread")]
async fn socks5_tcp_requires_the_configured_username_password() {
    use protocol::socks5::messages::sub_negotiations::{
        UsernamePasswordRequest, UsernamePasswordResponse, UsernamePasswordStatus,
    };
    let mut tasks = Tasks::new();
    let users = HashMap::from([(
        Arc::<[u8]>::from(&b"alice"[..]),
        Arc::<[u8]>::from(&b"hunter2"[..]),
    )]);
    let server_addr = spawn_socks5_tcp(&mut tasks, direct_route_table(), None, users).await;
    let req_msg = b"authenticated request";
    let resp_msg = b"authenticated response";
    let greet = spawn_tcp_greet(&mut tasks, req_msg, resp_msg, 1).await;

    let mut stream = TcpStream::connect(server_addr).await.unwrap();
    NegotiationRequest {
        methods: vec![MethodIdentifier::UsernamePassword],
    }
    .encode(&mut stream)
    .await
    .unwrap();
    let response = NegotiationResponse::decode(&mut stream).await.unwrap();
    assert_eq!(response.method, Some(MethodIdentifier::UsernamePassword));
    UsernamePasswordRequest::new(b"alice", b"hunter2")
        .unwrap()
        .encode(&mut stream)
        .await
        .unwrap();
    let auth = UsernamePasswordResponse::decode(&mut stream).await.unwrap();
    assert_eq!(auth.status, UsernamePasswordStatus::Success);
    let response = socks5_connect(&mut stream, greet.address.clone()).await;
    assert_eq!(response.reply, Reply::Succeeded);
    stream.write_all(req_msg).await.unwrap();
    read_exact_response(&mut stream, resp_msg).await;
}

/// A UDP origin that asserts each datagram equals `req` and answers `resp`.
async fn spawn_udp_greet(
    tasks: &mut Tasks,
    req: &[u8],
    resp: &[u8],
    accepts: usize,
) -> InternetAddr {
    let listener = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let req = req.to_vec();
    let resp = resp.to_vec();
    tasks.spawn(async move {
        for _ in 0..accepts {
            let mut buf = [0; 1024];
            let (len, addr) = listener.recv_from(&mut buf).await.unwrap();
            assert_eq!(&buf[..len], req);
            listener.send_to(&resp, addr).await.unwrap();
        }
    });
    addr.into()
}

async fn spawn_socks5_udp(tasks: &mut Tasks) -> std::net::SocketAddr {
    let handler = Socks5ServerUdpAccessConnHandler::new(
        RouteSelector::Empty,
        f64::INFINITY,
        udp_context(tasks),
    );
    let server = handler.build("127.0.0.1:0").await.unwrap();
    let addr = server.listener().local_addr().unwrap();
    let (set_conn_handler_tx, set_conn_handler_rx) = loading::replace_conn_handler_channel();
    tasks.spawn(async move {
        let _set_conn_handler_tx = set_conn_handler_tx;
        server.serve(set_conn_handler_rx).await.unwrap();
    });
    addr
}

async fn socks5_udp_request(destination: &InternetAddr, fragment: u8, payload: &[u8]) -> Vec<u8> {
    let mut datagram = Vec::new();
    UdpRequestHeader {
        fragment,
        destination: destination.clone(),
    }
    .encode(&mut datagram)
    .await
    .unwrap();
    datagram.extend_from_slice(payload);
    datagram
}

async fn decode_socks5_udp_response(datagram: &[u8]) -> (UdpRequestHeader, Vec<u8>) {
    let mut cursor = std::io::Cursor::new(datagram);
    let header = UdpRequestHeader::decode(&mut cursor).await.unwrap();
    let offset = cursor.position() as usize;
    (header, datagram[offset..].to_vec())
}

#[tokio::test(flavor = "multi_thread")]
async fn socks5_udp_associate_relays_datagrams_byte_exact() {
    let mut tasks = Tasks::new();
    let server_addr = spawn_socks5_udp(&mut tasks).await;
    let req_msg = b"ping through socks5 udp";
    let resp_msg = b"pong through socks5 udp";
    let greet_addr = spawn_udp_greet(&mut tasks, req_msg, resp_msg, 4).await;

    let client = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let datagram = socks5_udp_request(&greet_addr, 0, req_msg).await;
    client.send_to(&datagram, server_addr).await.unwrap();
    let mut buf = [0u8; 2048];
    let (n, _) = tokio::time::timeout(Duration::from_secs(10), client.recv_from(&mut buf))
        .await
        .expect("timed out waiting for the SOCKS5 UDP reply")
        .unwrap();
    let (header, payload) = decode_socks5_udp_response(&buf[..n]).await;
    assert_eq!(header.fragment, 0);
    assert_eq!(
        header.destination, greet_addr,
        "the reply must name the datagram's destination"
    );
    assert_eq!(payload, resp_msg);
}

#[tokio::test(flavor = "multi_thread")]
async fn socks5_udp_drops_a_fragmented_datagram_but_keeps_serving() {
    let mut tasks = Tasks::new();
    let server_addr = spawn_socks5_udp(&mut tasks).await;
    let req_msg = b"fragmented then whole";
    let resp_msg = b"whole reply";
    let greet_addr = spawn_udp_greet(&mut tasks, req_msg, resp_msg, 4).await;

    let client = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let fragmented = socks5_udp_request(&greet_addr, 1, req_msg).await;
    client.send_to(&fragmented, server_addr).await.unwrap();
    let mut buf = [0u8; 2048];
    let no_reply =
        tokio::time::timeout(Duration::from_millis(500), client.recv_from(&mut buf)).await;
    assert!(
        no_reply.is_err(),
        "a fragmented SOCKS5 UDP datagram must be dropped, got {no_reply:?}"
    );
    // The server is still live and routes the unfragmented datagram.
    let whole = socks5_udp_request(&greet_addr, 0, req_msg).await;
    client.send_to(&whole, server_addr).await.unwrap();
    let (n, _) = tokio::time::timeout(Duration::from_secs(10), client.recv_from(&mut buf))
        .await
        .expect("the server stopped serving after a fragmented datagram")
        .unwrap();
    let (_, payload) = decode_socks5_udp_response(&buf[..n]).await;
    assert_eq!(payload, resp_msg);
}
