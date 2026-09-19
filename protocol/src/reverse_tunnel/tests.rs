#![allow(clippy::disallowed_methods)]
use super::initiator::initiator_transport;
use super::*;
use ae::anti_replay::{ReplayValidator, TimeValidator};
use bytes::BytesMut;
use common::{
    anti_replay::{VALIDATOR_CAPACITY, VALIDATOR_TIME_FRAME},
    connect::{
        ConnectorConfig, ConnectorConfigReader, ConnectorResetSignal, connector_config_cell,
    },
    loading::{self, Serve},
    notify::Notify,
    proxy_runtime::{
        addr::{ReverseTunnelTransport, RouteAddr, RouteAddrStr},
        client::{
            stream,
            udp::{ProbeFlowEnd, UdpProxyClient, probe_rtt},
        },
        conn_handler::{SpeedLimit, stream::StreamProxyConnHandler, udp::UdpProxyConnHandler},
        connect::udp::UdpConnector,
        context::{Runtime, StreamRuntime, UdpRuntime},
    },
    route::{HopConfig, ProbeRtt, RouteChain},
    stream_runtime::pool::StreamConnPool,
    udp_runtime::PACKET_BUFFER_LENGTH,
};
use std::{net::SocketAddr, sync::Arc, time::Duration};
use swap::Swap;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
    task::JoinSet,
};
use tokio_chacha20::config::ConfigBuilder;

use crate::stream_proto::connect::build_concrete_stream_connector_table;

/// Actively-polled scope of test-owned background tasks. The test body
/// runs through [`TestScope::run`], which races it against `join_next()`
/// on the scope, so a background task that panics fails the test
/// immediately instead of being observed only when the scope is dropped.
/// A `spawn_required` actor completing before the body does is equally a
/// failure (the wrapper turns the completion into a panic); tasks that
/// end normally are drained silently (e.g. the origin server finishing
/// its single accept). Dropping the scope remains the abort backstop for
/// tasks still running when the body completes.
struct TestScope {
    tasks: JoinSet<()>,
}
impl TestScope {
    fn new() -> Self {
        Self {
            tasks: JoinSet::new(),
        }
    }
    fn spawn(&mut self, task: impl std::future::Future<Output = ()> + Send + 'static) {
        self.tasks.spawn(task);
    }
    /// Spawn a long-lived actor that must stay alive for the whole
    /// [`TestScope::run`] body (runtime actors, the responder server,
    /// the initiator). A normal completion while the body is still
    /// running panics the test with a message naming the task; a panic
    /// inside the future propagates unchanged.
    fn spawn_required(
        &mut self,
        name: &'static str,
        task: impl std::future::Future<Output = ()> + Send + 'static,
    ) {
        self.tasks.spawn(async move {
            task.await;
            panic!("required task '{name}' exited before the test body completed");
        });
    }
    async fn run<F: std::future::Future>(mut self, body: F) -> F::Output {
        tokio::pin!(body);
        loop {
            tokio::select! {
                biased;
                joined = self.tasks.join_next(), if !self.tasks.is_empty() => {
                    // A background task exited before the body. Re-raise
                    // any panic it surfaced immediately; a normal
                    // completion is a legitimate shutdown (e.g. the
                    // origin server finishing its single accept) and is
                    // drained silently.
                    let joined = joined.expect("background task exists");
                    joined.unwrap();
                }
                value = &mut body => {
                    // The body completed. Drain tasks that exited in the
                    // same poll cycle so a required actor that ended
                    // right as the body finished still fails the test.
                    while let Some(joined) = self.tasks.try_join_next() {
                        joined.unwrap();
                    }
                    return value;
                }
            }
        }
    }
}

#[test]
fn config_has_no_destination_field() {
    let config: ReverseTunnelConfig = serde_json::from_str(
            r#"{
                "initiator": [{ "name": "private-a", "responder_addr": "tcp://127.0.0.1:7000", "header_key": "aGVsbG8" }],
                "responder": [{ "listen_addr": "rtpmux://127.0.0.1:7000", "header_key": "aGVsbG8" }]
            }"#,
        )
        .unwrap();
    assert_eq!(config.initiator[0].name.as_ref(), "private-a");
    assert_eq!(
        config.responder[0].listen_addr.0.protocol.as_ref(),
        "rtpmux"
    );
}

#[test]
fn unsupported_physical_transports_are_rejected() {
    let addr: RouteAddr = "rtp://127.0.0.1:7000".parse().unwrap();
    assert!(matches!(
        initiator_transport(&addr),
        Err(BuildError::UnsupportedPhysicalTransport(_))
    ));
}

#[test]
fn responder_rejects_payload_key() {
    let error = serde_json::from_str::<ReverseTunnelConfig>(
            r#"{
                "responder": [{ "listen_addr": "tcp://127.0.0.1:7000", "header_key": "aGVsbG8", "payload_key": "aGVsbG8" }]
            }"#,
        )
        .unwrap_err();
    assert!(error.to_string().contains("payload_key"), "{error}");
}

#[test]
fn reverse_tunnel_rejects_removed_fec_option() {
    let error = serde_json::from_str::<ReverseTunnelConfig>(
            r#"{ "initiator": [{ "name": "private-a", "responder_addr": "rtpmux://127.0.0.1:7000", "header_key": "aGVsbG8", "fec": true }], "responder": [{ "listen_addr": "rtpmux://127.0.0.1:7000", "header_key": "aGVsbG8" }] }"#,
        )
        .unwrap_err();
    assert!(error.to_string().contains("fec"), "{error}");
}

fn initiator_config(
    name: &str,
    responder_addr: RouteAddr,
    header_key: ConfigBuilder,
    payload_key: Option<ConfigBuilder>,
) -> ReverseTunnelInitiatorConfig {
    ReverseTunnelInitiatorConfig {
        name: name.into(),
        responder_addr: RouteAddrStr(responder_addr),
        header_key,
        payload_key,
        allow_loopback: true,
    }
}

async fn test_stream_runtime(
    tasks: &mut TestScope,
    udp_connector: &UdpConnector,
    connector_config: ConnectorConfigReader,
) -> (StreamRuntime, common::session::SessionSpawner) {
    let (session_spawner, mut session_rx) = common::session::SessionSpawner::channel();
    tasks.spawn_required("session spawner", async move {
        let mut sessions = JoinSet::new();
        loop {
            tokio::select! {
                Some(session) = session_rx.recv() => {
                    sessions.spawn(session);
                }
                Some(result) = sessions.join_next() => {
                    result.unwrap().unwrap();
                }
                else => break,
            }
        }
    });
    let reset = ConnectorResetSignal(Notify::new());
    let mut connector_drivers = JoinSet::new();
    let connector_table = Arc::new(build_concrete_stream_connector_table(
        connector_config,
        reset,
        &mut connector_drivers,
        udp_connector,
    ));
    tasks.spawn_required("connector drivers", async move {
        while let Some(result) = connector_drivers.join_next().await {
            result.unwrap().unwrap();
        }
    });
    let (retention_actor, retention) = common::lifecycle::retention::RetentionActor::new();
    tasks.spawn_required("retention actor", async move {
        retention_actor.run().await;
    });
    (
        StreamRuntime {
            session_table: None,
            pool: Swap::new(StreamConnPool::empty()),
            connector_table,
            replay_validator: Arc::new(ReplayValidator::new(
                VALIDATOR_TIME_FRAME,
                VALIDATOR_CAPACITY,
            )),
            session_spawner: session_spawner.clone(),
            retention,
        },
        session_spawner,
    )
}

async fn test_runtime(tasks: &mut TestScope) -> (Runtime, common::session::SessionSpawner) {
    let (connector_config, _updater) = connector_config_cell(ConnectorConfig::default());
    let udp_connector = Arc::new(UdpConnector::new(connector_config.clone()));
    let (stream, session_spawner) =
        test_stream_runtime(tasks, &udp_connector, connector_config).await;
    let runtime = Runtime {
        stream: stream.clone(),
        udp: UdpRuntime {
            session_table: None,
            time_validator: Arc::new(TimeValidator::new(VALIDATOR_TIME_FRAME)),
            connector: udp_connector,
            session_spawner: session_spawner.clone(),
            retention: stream.retention.clone(),
        },
        session_spawner: session_spawner.clone(),
    };
    (runtime, session_spawner)
}

async fn origin(tasks: &mut TestScope) -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    // Transient actor: it handles ONE ping/pong exchange and then
    // completes mid-body, so it is a plain spawn (a normal completion is
    // drained silently) rather than `spawn_required`; a panic inside
    // still fails the test through the scope's join reaping.
    tasks.spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let mut request = [0; 4];
        stream.read_exact(&mut request).await.unwrap();
        assert_eq!(&request, b"ping");
        stream.write_all(b"pong").await.unwrap();
    });
    addr
}

fn initiator_handler(
    name: &str,
    responder_addr: RouteAddr,
    transport: ReverseTunnelTransport,
    crypto: tokio_chacha20::config::Config,
    runtime: Runtime,
) -> ReverseTunnelInitiatorHandler {
    let virtual_addr: Arc<str> = Arc::from(format!("{}://{name}", transport.protocol()));
    ReverseTunnelInitiatorHandler {
        name: name.into(),
        responder_addr,
        transport,
        registration_crypto: crypto.clone(),
        stream_proxy: Arc::new(StreamProxyConnHandler::new(
            crypto.clone(),
            None,
            runtime.stream.clone(),
            virtual_addr,
            true,
            SpeedLimit::UNLIMITED,
        )),
        udp_proxy: Arc::new(UdpProxyConnHandler::new(
            crypto,
            None,
            runtime.udp,
            true,
            SpeedLimit::UNLIMITED,
        )),
        stream_runtime: runtime.stream,
    }
}

async fn bind_rtp_responder(key: [u8; 32]) -> rtp_mux::RtpMuxServer {
    rtp_mux::RtpMuxServer::bind(
        "127.0.0.1:0",
        rtp_mux::RtpMuxServerConfig {
            obfuscation_key: Some(rtp_mux::ObfuscationKey::from_bytes(key)),
        },
    )
    .await
    .unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn invalid_payload_key_is_reported_as_payload_crypto() {
    let mut scope = TestScope::new();
    let (runtime, _session_spawner) = test_runtime(&mut scope).await;
    let config = initiator_config(
        "private-a",
        "tcp://127.0.0.1:7000".parse().unwrap(),
        ConfigBuilder("aGVsbG8".into()),
        Some(ConfigBuilder("c2VjcmV0LXByb3h5LWtleQ!!".into())),
    );
    scope
        .run(async {
            let error = ReverseTunnelInitiatorBuilder::new(config, runtime)
                .unwrap()
                .handler()
                .unwrap_err();
            assert!(matches!(error, BuildError::PayloadCrypto(_)), "{error}");
            assert!(!matches!(error, BuildError::HeaderCrypto(_)), "{error}");
        })
        .await;
}

async fn verify_reverse_proxy_hop_with_payload(transport: ReverseTunnelTransport) {
    let mut scope = TestScope::new();
    let (runtime, session_spawner) = test_runtime(&mut scope).await;
    let header_key = "aGVsbG8";
    let payload_key = "cGF5bG9hZA";
    let header_crypto = ConfigBuilder(header_key.into()).build().unwrap();
    let payload_crypto = ConfigBuilder(payload_key.into()).build().unwrap();
    let responder_handler = ReverseTunnelResponderHandler {
        registration_crypto: header_crypto.clone(),
        stream_runtime: runtime.stream.clone(),
        udp_runtime: runtime.udp.clone(),
    };
    let responder_addr = match transport {
        ReverseTunnelTransport::Tcp => {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            let server = TcpReverseTunnelResponder {
                listener,
                handler: responder_handler,
            };
            scope.spawn_required("tcp responder server", async move {
                let (_tx, rx) = loading::replace_conn_handler_channel();
                server.serve(rx).await.unwrap();
            });
            format!("tcp://{addr}").parse().unwrap()
        }
        ReverseTunnelTransport::Rtp => {
            let server = bind_rtp_responder(*header_crypto.key()).await;
            let addr = server.listener().local_addr();
            let server = RtpReverseTunnelResponder {
                server,
                handler: responder_handler,
                session_spawner,
            };
            scope.spawn_required("rtp responder server", async move {
                let (_tx, rx) = loading::replace_conn_handler_channel();
                server.serve(rx).await.unwrap();
            });
            format!("rtpmux://{addr}").parse().unwrap()
        }
    };
    let config = initiator_config(
        "private-a",
        responder_addr,
        ConfigBuilder(header_key.into()),
        Some(ConfigBuilder(payload_key.into())),
    );
    let initiator = ReverseTunnelInitiator::new(
        ReverseTunnelInitiatorBuilder::new(config, runtime.clone())
            .unwrap()
            .handler()
            .unwrap(),
    );
    scope.spawn_required("initiator", async move {
        let (_tx, rx) = loading::replace_conn_handler_channel();
        initiator.serve(rx).await.unwrap();
    });
    let origin_addr = origin(&mut scope).await;
    let reverse_addr: RouteAddr = format!("{}://private-a", transport.protocol())
        .parse()
        .unwrap();
    let chain = [HopConfig {
        name: None,

        address: reverse_addr,
        header_crypto: header_crypto.clone(),
        payload_crypto: Some(payload_crypto.clone()),
    }];
    let destination: RouteAddr = format!("tcp://{origin_addr}").parse().unwrap();
    scope
        .run(async {
            let mut stream = tokio::time::timeout(
                Duration::from_secs(10),
                stream::establish(&chain, destination, &runtime.stream),
            )
            .await
            .expect("reverse tunnel did not register")
            .unwrap()
            .stream;
            tokio::time::timeout(Duration::from_secs(10), async {
                stream.write_all(b"ping").await.unwrap();
                let mut response = [0; 4];
                stream.read_exact(&mut response).await.unwrap();
                assert_eq!(&response, b"pong");
            })
            .await
            .expect("encrypted reverse proxy stream stalled");
        })
        .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn tcp_reverse_tunnel_encrypts_payload() {
    verify_reverse_proxy_hop_with_payload(ReverseTunnelTransport::Tcp).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn rtp_reverse_tunnel_encrypts_payload() {
    verify_reverse_proxy_hop_with_payload(ReverseTunnelTransport::Rtp).await;
}

async fn verify_reverse_udp_proxy_hop(
    transport: ReverseTunnelTransport,
    behind_regular_udp_hop: bool,
) {
    let mut scope = TestScope::new();
    let (runtime, session_spawner) = test_runtime(&mut scope).await;
    let header_key = "aGVsbG8";
    let payload_key = "cGF5bG9hZA";
    let header_crypto = ConfigBuilder(header_key.into()).build().unwrap();
    let payload_crypto = ConfigBuilder(payload_key.into()).build().unwrap();
    let responder_handler = ReverseTunnelResponderHandler {
        registration_crypto: header_crypto.clone(),
        stream_runtime: runtime.stream.clone(),
        udp_runtime: runtime.udp.clone(),
    };
    let responder_addr = match transport {
        ReverseTunnelTransport::Tcp => {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            let server = TcpReverseTunnelResponder {
                listener,
                handler: responder_handler,
            };
            scope.spawn_required("tcp responder server", async move {
                let (_tx, rx) = loading::replace_conn_handler_channel();
                server.serve(rx).await.unwrap();
            });
            format!("tcp://{addr}").parse().unwrap()
        }
        ReverseTunnelTransport::Rtp => {
            let server = bind_rtp_responder(*header_crypto.key()).await;
            let addr = server.listener().local_addr();
            let server = RtpReverseTunnelResponder {
                server,
                handler: responder_handler,
                session_spawner,
            };
            scope.spawn_required("rtp responder server", async move {
                let (_tx, rx) = loading::replace_conn_handler_channel();
                server.serve(rx).await.unwrap();
            });
            format!("rtpmux://{addr}").parse().unwrap()
        }
    };
    let config = initiator_config(
        "private-udp",
        responder_addr,
        ConfigBuilder(header_key.into()),
        Some(ConfigBuilder(payload_key.into())),
    );
    let initiator = ReverseTunnelInitiator::new(
        ReverseTunnelInitiatorBuilder::new(config, runtime.clone())
            .unwrap()
            .handler()
            .unwrap(),
    );
    scope.spawn_required("initiator", async move {
        let (_tx, rx) = loading::replace_conn_handler_channel();
        initiator.serve(rx).await.unwrap();
    });
    let origin = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let origin_addr = origin.local_addr().unwrap();
    scope.spawn(async move {
        let mut packet = [0; 64];
        for (request, response) in [(b"ping".as_slice(), b"pong".as_slice()), (b"next", b"done")] {
            let (n, peer) = origin.recv_from(&mut packet).await.unwrap();
            assert_eq!(&packet[..n], request);
            origin.send_to(response, peer).await.unwrap();
        }
    });
    let reverse_addr: RouteAddr = format!("{}://private-udp", transport.protocol())
        .parse()
        .unwrap();
    let mut chain = Vec::new();
    if behind_regular_udp_hop {
        let first_header_crypto = tokio_chacha20::config::Config::new([9; 32].into());
        let first_payload_crypto = tokio_chacha20::config::Config::new([10; 32].into());
        let server = UdpProxyConnHandler::new(
            first_header_crypto.clone(),
            Some(first_payload_crypto.clone()),
            runtime.udp.clone(),
            true,
            SpeedLimit::UNLIMITED,
        )
        .build("127.0.0.1:0")
        .await
        .unwrap();
        let server_addr = server.listener().local_addr().unwrap();
        scope.spawn_required("regular UDP proxy server", async move {
            let (_tx, rx) = loading::replace_conn_handler_channel();
            server.serve(rx).await.unwrap();
        });
        chain.push(HopConfig {
            name: None,

            address: RouteAddr::udp(server_addr.into()),
            header_crypto: first_header_crypto,
            payload_crypto: Some(first_payload_crypto),
        });
    }
    chain.push(HopConfig {
        name: None,

        address: reverse_addr,
        header_crypto,
        payload_crypto: Some(payload_crypto),
    });
    let chain: Arc<RouteChain> = chain.into();
    scope
        .run(async {
            let client = tokio::time::timeout(
                Duration::from_secs(10),
                UdpProxyClient::establish(chain, origin_addr.into(), &runtime.udp),
            )
            .await
            .expect("UDP reverse tunnel did not register")
            .unwrap();
            let (mut read, mut write) = client.into_split();
            tokio::time::timeout(Duration::from_secs(10), async {
                write.send(b"ping").await.unwrap();
                let mut response = [0; 4];
                let n = read.recv(&mut response).await.unwrap();
                assert_eq!(n, response.len());
                assert_eq!(&response, b"pong");
                write.send(b"next").await.unwrap();
                let n = read.recv(&mut response).await.unwrap();
                assert_eq!(n, response.len());
                assert_eq!(&response, b"done");
            })
            .await
            .expect("encrypted UDP reverse proxy flow stalled");
        })
        .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn tcp_reverse_tunnel_carries_udp_with_payload_encryption() {
    verify_reverse_udp_proxy_hop(ReverseTunnelTransport::Tcp, false).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn rtp_reverse_tunnel_is_a_later_udp_hop_with_per_hop_payload_encryption() {
    verify_reverse_udp_proxy_hop(ReverseTunnelTransport::Rtp, true).await;
}

/// A probe over a reverse tunnel on the rtp mux transport, driven through
/// the production [`probe_rtt`], must close the echo flow promptly once
/// its write half is shut down: the tunnel's mux stream propagates the
/// clean EOF to the initiator's echo flow, whose response writer closes,
/// observed as a clean EOF on the probe's read half and reported through
/// the flow-completion signal rather than a fixed sleep.
#[tokio::test(flavor = "multi_thread")]
async fn rtp_reverse_tunnel_probe_closes_the_flow_promptly() {
    let mut scope = TestScope::new();
    let (runtime, session_spawner) = test_runtime(&mut scope).await;
    let crypto = tokio_chacha20::config::Config::new([7; 32].into());
    let responder_handler = ReverseTunnelResponderHandler {
        registration_crypto: crypto.clone(),
        stream_runtime: runtime.stream.clone(),
        udp_runtime: runtime.udp.clone(),
    };
    let server = bind_rtp_responder(*crypto.key()).await;
    let addr = server.listener().local_addr();
    let server = RtpReverseTunnelResponder {
        server,
        handler: responder_handler,
        session_spawner,
    };
    scope.spawn_required("rtp responder server", async move {
        let (_tx, rx) = loading::replace_conn_handler_channel();
        server.serve(rx).await.unwrap();
    });
    let responder_addr: RouteAddr = format!("rtpmux://{addr}").parse().unwrap();
    let initiator = ReverseTunnelInitiator::new(initiator_handler(
        "private-udp",
        responder_addr,
        ReverseTunnelTransport::Rtp,
        crypto.clone(),
        runtime.clone(),
    ));
    scope.spawn_required("initiator", async move {
        let (_tx, rx) = loading::replace_conn_handler_channel();
        initiator.serve(rx).await.unwrap();
    });
    let chain: Arc<RouteChain> = Arc::from([HopConfig {
        name: None,

        address: "revtunrtp://private-udp".parse().unwrap(),
        header_crypto: crypto,
        payload_crypto: None,
    }]);
    scope
        .run(async {
            let mut pkt_buf = BytesMut::with_capacity(PACKET_BUFFER_LENGTH);
            let (rtt, probe_epilog) = tokio::time::timeout(
                Duration::from_secs(30),
                probe_rtt(&mut pkt_buf, &chain, &runtime.udp),
            )
            .await
            .expect("timed out probing the reverse tunnel")
            .unwrap();
            assert!(rtt > Duration::ZERO);
            assert!(rtt < Duration::from_secs(1));
            // The teardown epilog is owned by the caller: spawn it into a
            // scoped JoinSet, as `probe_task` does in production, so the
            // flow's end is observed and reported.
            let mut epilogs = tokio::task::JoinSet::new();
            if let Some(fut) = probe_epilog.fut {
                epilogs.spawn(fut);
            }
            let mut flow_end = probe_epilog.end;

            // Await the actual flow termination: the completion signal must
            // report a clean EOF well under the safety timeout.
            let end = tokio::time::timeout(Duration::from_secs(3), async {
                loop {
                    let value = *flow_end.borrow_and_update();
                    if value != ProbeFlowEnd::Pending {
                        return value;
                    }
                    flow_end.changed().await.unwrap();
                }
            })
            .await
            .expect("the reverse-tunnel echo flow must terminate promptly");
            assert_eq!(end, ProbeFlowEnd::Eof);
        })
        .await;
}

async fn verify_reverse_proxy_hop(transport: ReverseTunnelTransport) {
    let mut scope = TestScope::new();
    let (runtime, session_spawner) = test_runtime(&mut scope).await;
    let crypto = tokio_chacha20::config::Config::new([7; 32].into());
    let responder_handler = ReverseTunnelResponderHandler {
        registration_crypto: crypto.clone(),
        stream_runtime: runtime.stream.clone(),
        udp_runtime: runtime.udp.clone(),
    };
    let responder_addr = match transport {
        ReverseTunnelTransport::Tcp => {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            let server = TcpReverseTunnelResponder {
                listener,
                handler: responder_handler,
            };
            scope.spawn_required("tcp responder server", async move {
                let (_tx, rx) = loading::replace_conn_handler_channel();
                server.serve(rx).await.unwrap();
            });
            format!("tcp://{addr}").parse().unwrap()
        }
        ReverseTunnelTransport::Rtp => {
            let server = bind_rtp_responder(*crypto.key()).await;
            let addr = server.listener().local_addr();
            let server = RtpReverseTunnelResponder {
                server,
                handler: responder_handler,
                session_spawner,
            };
            scope.spawn_required("rtp responder server", async move {
                let (_tx, rx) = loading::replace_conn_handler_channel();
                server.serve(rx).await.unwrap();
            });
            format!("rtpmux://{addr}").parse().unwrap()
        }
    };
    let initiator = ReverseTunnelInitiator::new(initiator_handler(
        "private-a",
        responder_addr,
        transport,
        crypto.clone(),
        runtime.clone(),
    ));
    scope.spawn_required("initiator", async move {
        let (_tx, rx) = loading::replace_conn_handler_channel();
        initiator.serve(rx).await.unwrap();
    });
    let origin_addr = origin(&mut scope).await;
    let reverse_addr: RouteAddr = format!("{}://private-a", transport.protocol())
        .parse()
        .unwrap();
    let chain = [HopConfig {
        name: None,

        address: reverse_addr,
        header_crypto: crypto,
        payload_crypto: None,
    }];
    let destination: RouteAddr = format!("tcp://{origin_addr}").parse().unwrap();
    scope
        .run(async {
            let mut stream = tokio::time::timeout(
                Duration::from_secs(10),
                stream::establish(&chain, destination, &runtime.stream),
            )
            .await
            .expect("reverse tunnel did not register")
            .unwrap()
            .stream;
            tokio::time::timeout(Duration::from_secs(10), async {
                stream.write_all(b"ping").await.unwrap();
                let mut response = [0; 4];
                stream.read_exact(&mut response).await.unwrap();
                assert_eq!(&response, b"pong");
            })
            .await
            .expect("reverse proxy stream stalled");
            // The registered tunnel is addressable by name on both the stream
            // and UDP connector tables; each must report its peer and uptime.
            let tracer = stream::StreamTracer::new(runtime.stream.clone());
            let stream_stats = tracer.session_stats(&chain).await;
            assert!(
                stream_stats
                    .as_deref()
                    .is_some_and(|s| s.starts_with("peer=")),
                "stream named-session stats: {stream_stats:?}"
            );
            let udp_stats = runtime
                .udp
                .connector
                .named_session_stats(transport.protocol(), "private-a");
            assert!(
                udp_stats.as_deref().is_some_and(|s| s.starts_with("peer=")),
                "udp named-session stats: {udp_stats:?}"
            );
        })
        .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn tcp_reverse_tunnel_is_a_named_proxy_hop() {
    verify_reverse_proxy_hop(ReverseTunnelTransport::Tcp).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn rtp_reverse_tunnel_is_a_named_proxy_hop() {
    verify_reverse_proxy_hop(ReverseTunnelTransport::Rtp).await;
}

/// A clock that follows tokio's (paused) virtual time, so a task driven under
/// `start_paused` observes the same advance that `tokio::time::sleep`
/// produces. Lets the reconnect loop's session-stability boundary be placed
/// at an exact virtual instant instead of a real 30-second wait.
#[derive(Debug)]
struct VirtualClock {
    base: std::time::Instant,
    epoch: tokio::time::Instant,
}
impl VirtualClock {
    fn new() -> Self {
        Self {
            base: std::time::Instant::now(),
            epoch: tokio::time::Instant::now(),
        }
    }
}
impl common::clock::Clock for VirtualClock {
    fn now(&self) -> std::time::Instant {
        self.base + self.epoch.elapsed()
    }
}

/// The reconnect loop folds each session's measured stability into the backoff
/// at its `on_session_ended` call site, using the injected clock. Two short
/// sessions grow the backoff (250ms, then 500ms); a stable third session must
/// reset it, so the wait before the fourth session is the initial 250ms again.
/// Removing the call (or mis-guarding its `>=` stability check) leaves the
/// grown backoff in place, and the fourth session's start instant moves.
#[tokio::test(start_paused = true)]
async fn a_stable_session_resets_the_reconnect_backoff_at_the_serve_call_site() {
    use std::sync::Mutex;

    use super::initiator::{INITIAL_RECONNECT_DELAY, STABLE_SESSION, serve_initiator_session_loop};
    use common::clock::Clock;

    let mut scope = TestScope::new();
    let (runtime, _spawner) = test_runtime(&mut scope).await;
    let handler = initiator_handler(
        "private-a",
        "tcp://127.0.0.1:7000".parse().unwrap(),
        ReverseTunnelTransport::Tcp,
        tokio_chacha20::config::Config::new([7; 32].into()),
        runtime,
    );
    let clock = Arc::new(VirtualClock::new());
    let starts = Arc::new(Mutex::new(Vec::new()));
    let (tx, rx) = loading::replace_conn_handler_channel();
    let mut tx = Some(tx);
    let mut calls = 0usize;
    serve_initiator_session_loop(
        handler,
        Arc::clone(&clock) as Arc<dyn common::clock::Clock>,
        rx,
        {
            let clock = Arc::clone(&clock);
            let starts = Arc::clone(&starts);
            move |_handler| {
                let index = calls;
                calls += 1;
                starts.lock().unwrap().push(clock.now());
                if index == 3 {
                    // End the loop by dropping the replacement channel after the
                    // fourth session has started.
                    drop(tx.take());
                }
                let stable = index == 2;
                Box::pin(async move {
                    if stable {
                        tokio::time::sleep(STABLE_SESSION).await;
                    }
                    Err(super::wire::ReverseTunnelSessionError::Closed)
                })
            }
        },
    )
    .await
    .unwrap();

    let starts = starts.lock().unwrap().clone();
    assert_eq!(
        starts.len(),
        4,
        "the loop must run the four scripted sessions"
    );
    assert_eq!(
        starts[1] - starts[0],
        INITIAL_RECONNECT_DELAY,
        "the first post-session wait is the initial backoff"
    );
    assert_eq!(
        starts[2] - starts[1],
        INITIAL_RECONNECT_DELAY * 2,
        "a second short session doubles the backoff"
    );
    assert_eq!(
        starts[3] - starts[2],
        STABLE_SESSION + INITIAL_RECONNECT_DELAY,
        "a stable session must reset the backoff at the serve call site"
    );
}

/// A raw (non-initiator) client proves the responder validates the
/// registration request before it registers anything: an unsupported wire
/// version and an invalid tunnel name are each rejected with their own
/// `RegisterError` and no mux session is started.
#[tokio::test(flavor = "multi_thread")]
async fn responder_rejects_an_unsupported_version_and_an_invalid_name() {
    use super::wire::{REGISTER_VERSION, RegisterError, RegisterRequest, RegisterResponse};
    use ae::anti_replay::ValidatorRef;
    use common::header::codec::{timed_read_header_async, timed_write_header_async};

    let mut scope = TestScope::new();
    let (runtime, _session_spawner) = test_runtime(&mut scope).await;
    let crypto = tokio_chacha20::config::Config::new([9; 32].into());
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let handler = ReverseTunnelResponderHandler {
        registration_crypto: crypto.clone(),
        stream_runtime: runtime.stream.clone(),
        udp_runtime: runtime.udp.clone(),
    };
    let server = TcpReverseTunnelResponder { listener, handler };
    scope.spawn_required("tcp responder server", async move {
        let (_tx, rx) = loading::replace_conn_handler_channel();
        server.serve(rx).await.unwrap();
    });
    scope
        .run(async {
            let validator = ValidatorRef::Replay(&runtime.stream.replay_validator);
            for (version, name, expected) in [
                (
                    REGISTER_VERSION.wrapping_add(1),
                    "private-a",
                    RegisterError::UnsupportedVersion,
                ),
                (REGISTER_VERSION, "bad name!", RegisterError::InvalidName),
            ] {
                let mut client = tokio::net::TcpStream::connect(addr).await.unwrap();
                timed_write_header_async(
                    &mut client,
                    &RegisterRequest {
                        version,
                        name: name.into(),
                    },
                    *crypto.key(),
                    common::STREAM_IO_TIMEOUT,
                )
                .await
                .unwrap();
                let response: RegisterResponse = tokio::time::timeout(
                    Duration::from_secs(10),
                    timed_read_header_async(
                        &mut client,
                        *crypto.key(),
                        &validator,
                        common::STREAM_IO_TIMEOUT,
                    ),
                )
                .await
                .expect("timed out reading the register response")
                .unwrap();
                assert_eq!(
                    response.result,
                    Err(expected),
                    "version={version} name={name}"
                );
            }
        })
        .await;
}

/// The prepare pipeline rejects a duplicate initiator name and a duplicate
/// responder listen address with the offending key, and otherwise binds each
/// responder transport through its own builder.
#[tokio::test(flavor = "multi_thread")]
async fn prepare_reports_duplicate_keys_and_binds_both_responder_transports() {
    let mut scope = TestScope::new();
    let (runtime, _session_spawner) = test_runtime(&mut scope).await;
    let loader = ReverseTunnelLoader::new();
    let snapshot = loader.snapshot();
    scope
        .run(async {
            let header_key = || ConfigBuilder("aGVsbG8".into());
            let dup_initiators = ReverseTunnelConfig {
                initiator: vec![
                    initiator_config(
                        "private-a",
                        "tcp://127.0.0.1:1".parse().unwrap(),
                        header_key(),
                        None,
                    ),
                    initiator_config(
                        "private-a",
                        "tcp://127.0.0.1:2".parse().unwrap(),
                        header_key(),
                        None,
                    ),
                ],
                responder: vec![],
            };
            let err = match prepare(dup_initiators, &snapshot, runtime.clone()).await {
                Ok(_) => panic!("a duplicate initiator key must be rejected"),
                Err(err) => err,
            };
            let text = format!("{err}");
            assert!(
                text.contains("duplicate reverse tunnel configuration key")
                    && text.contains("private-a"),
                "{err}"
            );

            let responder = |addr: &str| ReverseTunnelResponderConfig {
                listen_addr: RouteAddrStr(addr.parse().unwrap()),
                header_key: header_key(),
            };
            let dup_responders = ReverseTunnelConfig {
                initiator: vec![],
                responder: vec![
                    responder("tcp://127.0.0.1:0"),
                    responder("tcp://127.0.0.1:0"),
                ],
            };
            let err = match prepare(dup_responders, &snapshot, runtime.clone()).await {
                Ok(_) => panic!("a duplicate responder key must be rejected"),
                Err(err) => err,
            };
            assert!(
                format!("{err}").contains("duplicate reverse tunnel configuration key"),
                "{err}"
            );

            for listen in ["tcp://127.0.0.1:0", "rtpmux://127.0.0.1:0"] {
                let config = ReverseTunnelConfig {
                    initiator: vec![],
                    responder: vec![responder(listen)],
                };
                prepare(config, &snapshot, runtime.clone())
                    .await
                    .unwrap_or_else(|e| panic!("prepare failed for {listen}: {e}"));
            }
        })
        .await;
}
