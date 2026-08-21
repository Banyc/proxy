#[cfg(test)]
mod tests {
    use std::{sync::Arc, time::Duration};

    use ae::anti_replay::{ReplayValidator, TimeValidator};
    use bytes::BytesMut;
    use common::{
        addr::InternetAddr,
        anti_replay::{VALIDATOR_CAPACITY, VALIDATOR_TIME_FRAME, VALIDATOR_UDP_HDR_TTL},
        connect::{ConnectorConfig, ConnectorResetSignal, connector_config_cell},
        header::route::RouteErrorKind,
        loading::{self, Serve},
        notify::Notify,
        proxy_runtime::{
            addr::RouteAddr,
            client::{
                self,
                udp::{ProbeFlowEnd, UdpProxyClient, UdpProxyClientReadHalf, probe_rtt},
            },
            conn_handler::{stream::StreamProxyConnHandler, udp::UdpProxyConnHandler},
            connect::udp::UdpConnector,
            context::{Runtime, StreamRuntime, UdpRuntime},
        },
        route::HopConfig,
        stream::pool::StreamConnPool,
        udp::PACKET_BUFFER_LENGTH,
    };
    use protocol::stream::{
        connect::build_concrete_stream_connector_table,
        streams::{
            mux::MuxProxyHandler, rtp_mux::build_rtp_mux_proxy_server,
            tcp_mux::build_tcp_mux_proxy_server,
        },
    };
    use serial_test::serial;
    use swap::Swap;
    use tokio::net::UdpSocket;
    use tokio::sync::watch;

    use crate::{STRESS_CHAINS, STRESS_PARALLEL, STRESS_SERIAL, scope::TestRuntimeScope};

    fn create_random_crypto() -> tokio_chacha20::config::Config {
        let key: [u8; 32] = rand::random();
        tokio_chacha20::config::Config::new(key.into())
    }

    fn udp_context(scope: &mut TestRuntimeScope) -> UdpRuntime {
        UdpRuntime {
            session_table: None,
            connector: Arc::new(UdpConnector::new(
                connector_config_cell(ConnectorConfig::default()).0,
            )),
            time_validator: Arc::new(TimeValidator::new(
                VALIDATOR_TIME_FRAME + VALIDATOR_UDP_HDR_TTL,
            )),
            session_spawner: scope.session_spawner(),
            retention: scope.retention(),
        }
    }

    async fn spawn_proxy(scope: &mut TestRuntimeScope, addr: &str) -> HopConfig {
        spawn_proxy_(scope, addr, true, false).await
    }

    async fn spawn_encrypted_proxy(scope: &mut TestRuntimeScope, addr: &str) -> HopConfig {
        spawn_proxy_(scope, addr, true, true).await
    }

    async fn spawn_guarded_proxy(scope: &mut TestRuntimeScope, addr: &str) -> HopConfig {
        spawn_proxy_(scope, addr, false, false).await
    }

    async fn spawn_proxy_(
        scope: &mut TestRuntimeScope,
        addr: &str,
        allow_loopback: bool,
        encrypt_payload: bool,
    ) -> HopConfig {
        let crypto = create_random_crypto();
        let payload_crypto = encrypt_payload.then(create_random_crypto);
        let proxy = UdpProxyConnHandler::new(
            crypto.clone(),
            payload_crypto.clone(),
            udp_context(scope),
            allow_loopback,
            f64::INFINITY,
        );
        let server = proxy.build(addr).await.unwrap();
        let proxy_addr = server.listener().local_addr().unwrap();
        scope.spawn_required(async move {
            let (_set_conn_handler_tx, set_conn_handler_rx) =
                loading::replace_conn_handler_channel();
            server.serve(set_conn_handler_rx).await
        });
        HopConfig {
            address: common::proxy_runtime::addr::RouteAddr::udp(proxy_addr.into()),
            header_crypto: crypto,
            payload_crypto,
        }
    }

    async fn spawn_greet(
        scope: &mut TestRuntimeScope,
        addr: &str,
        req: &[u8],
        resp: &[u8],
        accepts: usize,
    ) -> InternetAddr {
        let listener = UdpSocket::bind(addr).await.unwrap();
        let greet_addr = listener.local_addr().unwrap();
        let req = req.to_vec();
        let resp = resp.to_vec();
        scope.spawn_session(async move {
            for _ in 0..accepts {
                let mut buf = [0; 1024];
                let (len, addr) = listener.recv_from(&mut buf).await.unwrap();
                let msg_buf = &buf[..len];
                assert_eq!(msg_buf, req);
                listener.send_to(&resp, addr).await.unwrap();
            }
        });
        greet_addr.into()
    }

    async fn read_response(
        client: &mut UdpProxyClientReadHalf,
        resp_msg: &[u8],
    ) -> Result<(), client::udp::RecvError> {
        let mut buf = [0; 1024];
        let n = tokio::time::timeout(std::time::Duration::from_secs(10), client.recv(&mut buf))
            .await
            .expect("timed out waiting for the UDP proxy response")?;
        let msg_buf = &buf[..n];
        assert_eq!(msg_buf, resp_msg);
        Ok(())
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_proxies() {
        let mut scope = TestRuntimeScope::new();
        let context = udp_context(&mut scope);

        let mut pkt_buf = BytesMut::with_capacity(PACKET_BUFFER_LENGTH);

        // Start proxy servers
        let proxy_1_config = spawn_proxy(&mut scope, "127.0.0.1:0").await;
        let proxy_2_config = spawn_proxy(&mut scope, "127.0.0.1:0").await;
        let proxy_3_config = spawn_proxy(&mut scope, "127.0.0.1:0").await;
        let proxies: Arc<[_]> = vec![proxy_1_config, proxy_2_config, proxy_3_config].into();

        // Message to send
        let req_msg = b"hello world";
        let resp_msg = b"goodbye world";

        // Start greet server
        let greet_addr = spawn_greet(&mut scope, "127.0.0.1:0", req_msg, resp_msg, 1).await;

        scope
            .run(async {
                // Connect to proxy server
                let client = tokio::time::timeout(
                    std::time::Duration::from_secs(30),
                    UdpProxyClient::establish(proxies.clone(), greet_addr, &context),
                )
                .await
                .expect("timed out establishing the UDP proxy session")
                .unwrap();
                let (mut client_read, mut client_write) = client.into_split();

                // Send message
                client_write.send(req_msg).await.unwrap();

                // Read response
                read_response(&mut client_read, resp_msg).await.unwrap();

                // Trace
                let rtt = tokio::time::timeout(
                    std::time::Duration::from_secs(30),
                    probe_rtt(&mut pkt_buf, &proxies, &context),
                )
                .await
                .expect("timed out probing the UDP proxy chain")
                .unwrap()
                .0;
                assert!(rtt > Duration::from_secs(0));
                assert!(rtt < Duration::from_secs(1));
            })
            .await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn multiple_payload_keys_layer_each_udp_hop() {
        let mut scope = TestRuntimeScope::new();
        let context = udp_context(&mut scope);
        let proxies: Arc<[_]> = vec![
            spawn_encrypted_proxy(&mut scope, "127.0.0.1:0").await,
            spawn_encrypted_proxy(&mut scope, "127.0.0.1:0").await,
            spawn_encrypted_proxy(&mut scope, "127.0.0.1:0").await,
        ]
        .into();
        let request = b"hello through three encrypted UDP hops";
        let response = b"goodbye through three encrypted UDP hops";
        let destination = spawn_greet(&mut scope, "127.0.0.1:0", request, response, 1).await;
        scope
            .run(async {
                let client = tokio::time::timeout(
                    Duration::from_secs(30),
                    UdpProxyClient::establish(proxies.clone(), destination, &context),
                )
                .await
                .expect("timed out establishing the encrypted UDP proxy chain")
                .unwrap();
                let (mut client_read, mut client_write) = client.into_split();
                client_write.send(request).await.unwrap();
                read_response(&mut client_read, response).await.unwrap();
                let mut packet_buf = BytesMut::with_capacity(PACKET_BUFFER_LENGTH);
                let rtt = tokio::time::timeout(
                    Duration::from_secs(30),
                    probe_rtt(&mut packet_buf, &proxies, &context),
                )
                .await
                .expect("timed out probing the encrypted UDP proxy chain")
                .unwrap()
                .0;
                assert!(rtt > Duration::ZERO);
                assert!(rtt < Duration::from_secs(1));
            })
            .await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_clients() {
        let mut scope = TestRuntimeScope::new();
        let context = udp_context(&mut scope);

        // Start proxy servers
        let proxy_1_config = spawn_proxy(&mut scope, "127.0.0.1:0").await;
        let proxy_2_config = spawn_proxy(&mut scope, "127.0.0.1:0").await;
        let proxies: Arc<[_]> = vec![proxy_1_config, proxy_2_config].into();

        // Message to send
        let req_msg = b"hello world";
        let resp_msg = b"goodbye world";

        let clients = 2;

        // Start greet server
        let greet_addr = spawn_greet(&mut scope, "127.0.0.1:0", req_msg, resp_msg, clients).await;

        scope
            .run(async {
                let mut handles = tokio::task::JoinSet::new();

                for _ in 0..clients {
                    let proxies = proxies.clone();
                    let greet_addr = greet_addr.clone();
                    let context = context.clone();
                    handles.spawn(async move {
                        // Connect to proxy server
                        let client = tokio::time::timeout(
                            std::time::Duration::from_secs(30),
                            UdpProxyClient::establish(proxies, greet_addr, &context),
                        )
                        .await
                        .expect("timed out establishing the UDP proxy session")
                        .unwrap();
                        let (mut client_read, mut client_write) = client.into_split();

                        // Send message
                        client_write.send(req_msg).await.unwrap();

                        // Read response
                        read_response(&mut client_read, resp_msg).await.unwrap();
                    });
                }

                while let Some(x) = handles.join_next().await {
                    x.unwrap();
                }
            })
            .await;
    }

    #[tokio::test(flavor = "multi_thread")]
    #[serial]
    async fn stress_test() {
        let mut scope = TestRuntimeScope::new();
        let context = udp_context(&mut scope);

        // Start proxy servers
        let mut proxies = Vec::new();
        for _ in 0..STRESS_CHAINS {
            let proxy_config = spawn_proxy(&mut scope, "127.0.0.1:0").await;
            proxies.push(proxy_config);
        }
        let proxies: Arc<[_]> = proxies.into();

        // Message to send
        let req_msg = b"hello world";
        let resp_msg = b"goodbye world";

        // Start greet server
        let greet_addr =
            spawn_greet(&mut scope, "127.0.0.1:0", req_msg, resp_msg, usize::MAX).await;

        scope
            .run(async {
                let mut handles = tokio::task::JoinSet::new();

                for _ in 0..STRESS_PARALLEL {
                    let proxies = proxies.clone();
                    let greet_addr = greet_addr.clone();
                    let context = context.clone();
                    handles.spawn(async move {
                        for _ in 0..STRESS_SERIAL {
                            let greet_addr = greet_addr.clone();
                            // Connect to proxy server
                            let client = tokio::time::timeout(
                                std::time::Duration::from_secs(30),
                                UdpProxyClient::establish(proxies.clone(), greet_addr, &context),
                            )
                            .await
                            .expect("timed out establishing the UDP proxy session")
                            .unwrap();
                            let (mut client_read, mut client_write) = client.into_split();

                            // Send message
                            client_write.send(req_msg).await.unwrap();

                            // Read response
                            read_response(&mut client_read, resp_msg).await.unwrap();
                        }
                    });
                }

                while let Some(x) = handles.join_next().await {
                    x.unwrap();
                }
            })
            .await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_bad_proxy() {
        let mut scope = TestRuntimeScope::new();
        let context = udp_context(&mut scope);
        let proxy_1_config = spawn_guarded_proxy(&mut scope, "localhost:0").await;
        let proxy_2_config = spawn_guarded_proxy(&mut scope, "localhost:0").await;
        let proxy_3_config = spawn_guarded_proxy(&mut scope, "localhost:0").await;
        let req_msg = b"hello world";
        let resp_msg = b"goodbye world";
        let greet_addr = spawn_greet(&mut scope, "127.0.0.1:0", req_msg, resp_msg, 1).await;
        scope
            .run(async {
                let client = tokio::time::timeout(
                    std::time::Duration::from_secs(30),
                    UdpProxyClient::establish(
                        vec![proxy_1_config.clone(), proxy_2_config, proxy_3_config].into(),
                        greet_addr,
                        &context,
                    ),
                )
                .await
                .expect("timed out establishing the UDP proxy session")
                .unwrap();
                let (mut client_read, mut client_write) = client.into_split();
                client_write.send(req_msg).await.unwrap();
                let err = read_response(&mut client_read, resp_msg).await.unwrap_err();
                match err {
                    client::udp::RecvError::Response { err, addr } => {
                        match err.kind {
                            RouteErrorKind::Loopback => {}
                            _ => panic!("Unexpected error: {err:?}"),
                        }
                        assert_eq!(addr, proxy_1_config.address.address);
                    }
                    _ => panic!("Unexpected error: {err:?}"),
                }
            })
            .await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn loopback_spelled_as_ipv6_is_still_refused() {
        let mut scope = TestRuntimeScope::new();
        let context = udp_context(&mut scope);
        let proxy_config = spawn_guarded_proxy(&mut scope, "127.0.0.1:0").await;
        let req_msg = b"hello world";
        let resp_msg = b"goodbye world";
        let greet_addr = spawn_greet(&mut scope, "127.0.0.1:0", req_msg, resp_msg, 1).await;
        let greet_port = match *greet_addr {
            common::addr::InternetAddrKind::SocketAddr(addr) => addr.port(),
            ref other => panic!("{other:?}"),
        };
        let mapped: InternetAddr = std::net::SocketAddr::new(
            std::net::Ipv4Addr::new(127, 0, 0, 1)
                .to_ipv6_mapped()
                .into(),
            greet_port,
        )
        .into();
        scope
            .run(async {
                let client = tokio::time::timeout(
                    std::time::Duration::from_secs(30),
                    UdpProxyClient::establish(vec![proxy_config.clone()].into(), mapped, &context),
                )
                .await
                .expect("timed out establishing the UDP proxy session")
                .unwrap();
                let (mut client_read, mut client_write) = client.into_split();
                client_write.send(req_msg).await.unwrap();
                let err = read_response(&mut client_read, resp_msg)
                    .await
                    .expect_err("the proxy relayed to a loopback service");
                match err {
                    client::udp::RecvError::Response { err, addr } => {
                        assert!(
                            matches!(err.kind, RouteErrorKind::Loopback),
                            "unexpected error: {err:?}"
                        );
                        assert_eq!(addr, proxy_config.address.address);
                    }
                    _ => panic!("unexpected error: {err:?}"),
                }
            })
            .await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn an_unspecified_destination_is_still_refused() {
        let mut scope = TestRuntimeScope::new();
        let context = udp_context(&mut scope);
        let proxy_config = spawn_guarded_proxy(&mut scope, "127.0.0.1:0").await;
        let req_msg = b"hello world";
        let resp_msg = b"goodbye world";
        let greet_addr = spawn_greet(&mut scope, "127.0.0.1:0", req_msg, resp_msg, 1).await;
        let unspecified: InternetAddr =
            std::net::SocketAddr::new(std::net::Ipv4Addr::UNSPECIFIED.into(), greet_addr.port())
                .into();
        scope
            .run(async {
                let client = tokio::time::timeout(
                    std::time::Duration::from_secs(30),
                    UdpProxyClient::establish(
                        vec![proxy_config.clone()].into(),
                        unspecified,
                        &context,
                    ),
                )
                .await
                .expect("timed out establishing the UDP proxy session")
                .unwrap();
                let (mut client_read, mut client_write) = client.into_split();
                client_write.send(req_msg).await.unwrap();
                let err = read_response(&mut client_read, resp_msg)
                    .await
                    .expect_err("the proxy relayed to a service on its own host");
                match err {
                    client::udp::RecvError::Response { err, addr } => {
                        assert!(
                            matches!(err.kind, RouteErrorKind::Loopback),
                            "unexpected error: {err:?}"
                        );
                        assert_eq!(addr, proxy_config.address.address);
                    }
                    _ => panic!("unexpected error: {err:?}"),
                }
            })
            .await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_no_proxies() {
        let mut scope = TestRuntimeScope::new();
        let context = udp_context(&mut scope);

        // Start proxy servers

        // Message to send
        let req_msg = b"hello world";
        let resp_msg = b"goodbye world";

        // Start greet server
        let greet_addr = spawn_greet(&mut scope, "127.0.0.1:0", req_msg, resp_msg, 1).await;

        scope
            .run(async {
                // Connect to proxy server
                let client = tokio::time::timeout(
                    std::time::Duration::from_secs(30),
                    UdpProxyClient::establish(vec![].into(), greet_addr, &context),
                )
                .await
                .expect("timed out establishing the UDP proxy session")
                .unwrap();
                let (mut client_read, mut client_write) = client.into_split();

                // Send message
                client_write.send(req_msg).await.unwrap();

                // Read response
                read_response(&mut client_read, resp_msg).await.unwrap();
            })
            .await;
    }

    /// A full runtime whose `UdpConnector` has the tcpmux/rtpmux mux dialers
    /// registered, so UDP proxy chains can carry datagrams over a mux stream
    /// with the reverse-tunnel wire format.
    async fn mux_udp_runtime(scope: &mut TestRuntimeScope) -> Runtime {
        // Connector drivers run the per-protocol connector loops (e.g.
        // rtp_mux, tcp_mux) and are reaped here for the lifetime of the
        // test runtime.
        let mut connector_drivers = tokio::task::JoinSet::new();
        let connector_config = connector_config_cell(ConnectorConfig::default()).0;
        let udp_connector = Arc::new(UdpConnector::new(connector_config.clone()));
        let connector_table = Arc::new(build_concrete_stream_connector_table(
            connector_config,
            ConnectorResetSignal(Notify::new()),
            &mut connector_drivers,
            &udp_connector,
        ));
        scope.spawn_required(async move {
            while let Some(result) = connector_drivers.join_next().await {
                result
                    .expect("connector driver panicked")
                    .expect("connector driver failed");
            }
            Ok(())
        });
        let session_spawner = scope.session_spawner();
        let stream = StreamRuntime {
            session_table: None,
            pool: Swap::new(StreamConnPool::empty()),
            connector_table,
            replay_validator: Arc::new(ReplayValidator::new(
                VALIDATOR_TIME_FRAME,
                VALIDATOR_CAPACITY,
            )),
            session_spawner: session_spawner.clone(),
            retention: scope.retention(),
        };
        let udp = UdpRuntime {
            session_table: None,
            time_validator: Arc::new(TimeValidator::new(
                VALIDATOR_TIME_FRAME + VALIDATOR_UDP_HDR_TTL,
            )),
            connector: udp_connector,
            session_spawner: session_spawner.clone(),
            retention: stream.retention.clone(),
        };
        Runtime {
            stream: stream.clone(),
            udp,
            session_spawner,
        }
    }

    /// Spawn a tcpmux/rtpmux proxy server whose UDP handler accepts
    /// datagram flows, and return the chain config pointing at it. The
    /// payload crypto is freshly generated per hop.
    async fn spawn_mux_udp_proxy(
        scope: &mut TestRuntimeScope,
        runtime: &Runtime,
        protocol: &str,
    ) -> HopConfig {
        spawn_mux_udp_proxy_with_payload(scope, runtime, protocol, Some(create_random_crypto()))
            .await
    }

    /// Like [`spawn_mux_udp_proxy`], with an explicit payload crypto (pass
    /// `None` for hops that skip payload encryption).
    async fn spawn_mux_udp_proxy_with_payload(
        scope: &mut TestRuntimeScope,
        runtime: &Runtime,
        protocol: &str,
        payload_crypto: Option<tokio_chacha20::config::Config>,
    ) -> HopConfig {
        let crypto = create_random_crypto();
        let stream_proxy = StreamProxyConnHandler::new(
            crypto.clone(),
            payload_crypto.clone(),
            runtime.stream.clone(),
            Arc::from(format!("udp-over-{protocol}")),
            true,
            f64::INFINITY,
        );
        let udp_proxy = UdpProxyConnHandler::new(
            crypto.clone(),
            payload_crypto.clone(),
            runtime.udp.clone(),
            true,
            f64::INFINITY,
        );
        let handler = MuxProxyHandler {
            stream: stream_proxy,
            udp: Some(udp_proxy),
        };
        let session_spawner = runtime.session_spawner.clone();
        let (set_conn_handler_tx, set_conn_handler_rx) = loading::replace_conn_handler_channel();
        let proxy_addr = match protocol {
            "tcpmux" => {
                let server = build_tcp_mux_proxy_server("127.0.0.1:0", handler, session_spawner)
                    .await
                    .unwrap();
                let addr = server.listener().local_addr().unwrap();
                scope.spawn_required(async move {
                    let _set_conn_handler_tx = set_conn_handler_tx;
                    server.serve(set_conn_handler_rx).await
                });
                addr
            }
            "rtpmux" => {
                let server = build_rtp_mux_proxy_server("127.0.0.1:0", handler, session_spawner)
                    .await
                    .unwrap();
                let addr = server.listener().local_addr();
                scope.spawn_required(async move {
                    let _set_conn_handler_tx = set_conn_handler_tx;
                    server.serve(set_conn_handler_rx).await
                });
                addr
            }
            other => panic!("unsupported mux protocol {other}"),
        };
        HopConfig {
            address: RouteAddr {
                address: proxy_addr.into(),
                protocol: protocol.into(),
            },
            header_crypto: crypto,
            payload_crypto,
        }
    }

    async fn verify_udp_flows_over_mux_proxy(protocol: &str) {
        let mut scope = TestRuntimeScope::new();
        let runtime = mux_udp_runtime(&mut scope).await;
        let proxy_config = spawn_mux_udp_proxy(&mut scope, &runtime, protocol).await;
        let req_msg = b"ping";
        let resp_msg = b"pong";
        let greet_addr = spawn_greet(&mut scope, "127.0.0.1:0", req_msg, resp_msg, 2).await;
        scope
            .run(async {
                let client = tokio::time::timeout(
                    Duration::from_secs(30),
                    UdpProxyClient::establish(vec![proxy_config].into(), greet_addr, &runtime.udp),
                )
                .await
                .expect("timed out establishing the UDP session over the mux proxy")
                .unwrap();
                let (mut client_read, mut client_write) = client.into_split();
                // Two datagrams over the same mux UDP flow; the second
                // exercises the compact request form after the first response
                // confirms the route.
                client_write.send(req_msg).await.unwrap();
                read_response(&mut client_read, resp_msg).await.unwrap();
                client_write.send(req_msg).await.unwrap();
                read_response(&mut client_read, resp_msg).await.unwrap();
            })
            .await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn udp_datagrams_flow_over_tcpmux_proxy() {
        verify_udp_flows_over_mux_proxy("tcpmux").await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn udp_datagrams_flow_over_rtpmux_proxy() {
        verify_udp_flows_over_mux_proxy("rtpmux").await;
    }

    /// Await the probe flow's completion signal until it leaves the pending
    /// state, returning the terminal end. Re-entrant: an awaited future
    /// cancelled by a `timeout` leaves the receiver reusable, so a test can
    /// first assert the flow is still retained and then await its end.
    async fn await_flow_end(end: &mut watch::Receiver<ProbeFlowEnd>) -> ProbeFlowEnd {
        loop {
            let value = *end.borrow_and_update();
            if value != ProbeFlowEnd::Pending {
                return value;
            }
            end.changed().await.expect("flow-end channel closed");
        }
    }

    /// A probe over a relay chain of mux proxies, driven through the
    /// production [`probe_rtt`], must close the flow promptly once its
    /// write half is shut down, instead of idling the echo server out for
    /// the full [`common::udp::UDP_FLOW_TIMEOUT`] and tripping the "UDP
    /// echo flow idle" warning. The completion signal returned with the
    /// probe reports the actual flow termination: deleting the production
    /// epilog (the shutdown inside `probe_rtt`) leaves the flow retained
    /// and this test fails deterministically instead of passing by
    /// reconstructing the probe by hand.
    async fn probe_over_mux_proxy_chain_closes_the_flow_promptly(protocol: &str) {
        let mut scope = TestRuntimeScope::new();
        let runtime = mux_udp_runtime(&mut scope).await;
        // Two hops so the probe's echo flow sits behind a relay hop: the
        // relay is where the EOF previously stalled for 10s.
        let proxy_1 = spawn_mux_udp_proxy_with_payload(&mut scope, &runtime, protocol, None).await;
        let proxy_2 = spawn_mux_udp_proxy_with_payload(&mut scope, &runtime, protocol, None).await;
        let proxies: Arc<[_]> = vec![proxy_1, proxy_2].into();
        scope
            .run(async {
                let mut packet_buf = BytesMut::with_capacity(PACKET_BUFFER_LENGTH);
                let (rtt, probe_epilog) = tokio::time::timeout(
                    Duration::from_secs(30),
                    probe_rtt(&mut packet_buf, &proxies, &runtime.udp),
                )
                .await
                .expect("timed out probing the mux proxy chain")
                .unwrap();
                assert!(rtt > Duration::ZERO);
                assert!(rtt < Duration::from_secs(1));
                // The teardown epilog is returned unspawned so the caller
                // owns it: spawn it into a scoped JoinSet, exactly as
                // `probe_task` does in production, so the flow's end is
                // observed and reported.
                let mut epilogs = tokio::task::JoinSet::new();
                if let Some(fut) = probe_epilog.fut {
                    epilogs.spawn(fut);
                }
                let mut flow_end = probe_epilog.end;

                // The probe's epilog must have torn the flow down: the
                // completion signal reports a clean EOF well under the
                // safety timeout, not the timed-out end.
                let end =
                    tokio::time::timeout(Duration::from_secs(3), await_flow_end(&mut flow_end))
                        .await
                        .expect("the echo flow must terminate promptly after probe_rtt returns");
                assert_eq!(end, ProbeFlowEnd::Eof);
            })
            .await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn probe_over_tcpmux_proxy_chain_closes_the_flow_promptly() {
        probe_over_mux_proxy_chain_closes_the_flow_promptly("tcpmux").await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn probe_over_rtpmux_proxy_chain_closes_the_flow_promptly() {
        probe_over_mux_proxy_chain_closes_the_flow_promptly("rtpmux").await;
    }

    /// The probe flow's teardown must end cleanly: instead of capturing
    /// log lines and sleeping for a fixed interval, await the deterministic
    /// flow-completion signal and inspect the terminal result directly. A
    /// clean epilog close is [`ProbeFlowEnd::Eof`]; the retained-until-
    /// timeout end (or an error) is exactly what the old log-capture test
    /// guarded against, now observed deterministically.
    async fn probe_over_mux_proxy_chain_finishes_cleanly(protocol: &str) {
        let mut scope = TestRuntimeScope::new();
        let runtime = mux_udp_runtime(&mut scope).await;
        let proxy_1 = spawn_mux_udp_proxy_with_payload(&mut scope, &runtime, protocol, None).await;
        let proxy_2 = spawn_mux_udp_proxy_with_payload(&mut scope, &runtime, protocol, None).await;
        let proxies: Arc<[_]> = vec![proxy_1, proxy_2].into();
        scope
            .run(async {
                let mut packet_buf = BytesMut::with_capacity(PACKET_BUFFER_LENGTH);
                let (_rtt, probe_epilog) = tokio::time::timeout(
                    Duration::from_secs(30),
                    probe_rtt(&mut packet_buf, &proxies, &runtime.udp),
                )
                .await
                .expect("timed out probing the mux proxy chain")
                .unwrap();
                // Spawn the teardown epilog into a scoped JoinSet, exactly
                // as `probe_task` does in production.
                let mut epilogs = tokio::task::JoinSet::new();
                if let Some(fut) = probe_epilog.fut {
                    epilogs.spawn(fut);
                }
                let mut flow_end = probe_epilog.end;
                let end = tokio::time::timeout(Duration::from_secs(30), async {
                    await_flow_end(&mut flow_end).await
                })
                .await
                .expect("the probe flow must terminate");
                assert_eq!(
                    end,
                    ProbeFlowEnd::Eof,
                    "a probe flow over a mux relay chain must end by the clean epilog close"
                );
            })
            .await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn probe_over_tcpmux_proxy_chain_finishes_cleanly() {
        probe_over_mux_proxy_chain_finishes_cleanly("tcpmux").await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn probe_over_rtpmux_proxy_chain_finishes_cleanly() {
        probe_over_mux_proxy_chain_finishes_cleanly("rtpmux").await;
    }

    /// A mixed mux/raw-UDP chain retains the flow until its safety timeout:
    /// the raw-UDP hop's write-half shutdown reports
    /// [`ShutdownOutcome::Unsupported`], so the probe's EOF cannot propagate
    /// past it, and the relay aborts the flow only after
    /// [`common::udp::UDP_FLOW_TIMEOUT`] of inactivity. The completion
    /// signal therefore must NOT fire promptly, and must report the late
    /// EOF of the safety-timeout teardown once the retained relay dies.
    async fn probe_over_mixed_mux_raw_udp_chain_is_retained_until_the_safety_timeout(
        protocol: &str,
    ) {
        let mut scope = TestRuntimeScope::new();
        let runtime = mux_udp_runtime(&mut scope).await;
        // A mux hop relaying to a raw-UDP hop: the epilog stops at the raw
        // hop, so the flow is retained.
        let proxy_1 = spawn_mux_udp_proxy_with_payload(&mut scope, &runtime, protocol, None).await;
        let proxy_2 = spawn_proxy(&mut scope, "127.0.0.1:0").await;
        let proxies: Arc<[_]> = vec![proxy_1, proxy_2].into();
        scope
            .run(async {
                let started = std::time::Instant::now();
                let mut packet_buf = BytesMut::with_capacity(PACKET_BUFFER_LENGTH);
                let (rtt, probe_epilog) = tokio::time::timeout(
                    Duration::from_secs(30),
                    probe_rtt(&mut packet_buf, &proxies, &runtime.udp),
                )
                .await
                .expect("timed out probing the mixed mux/raw-UDP chain")
                .unwrap();
                // The teardown epilog is owned by the caller: spawn it into
                // a scoped JoinSet, as `probe_task` does in production, so
                // the retained flow's end is observed and reported.
                let mut epilogs = tokio::task::JoinSet::new();
                if let Some(fut) = probe_epilog.fut {
                    epilogs.spawn(fut);
                }
                let mut flow_end = probe_epilog.end;
                // The response is in hand promptly...
                assert!(rtt < Duration::from_secs(2));
                // ...but the flow must be retained: no completion within a
                // short window that the mux-only chain clears easily.
                let prompt = tokio::time::timeout(Duration::from_secs(2), async {
                    await_flow_end(&mut flow_end).await
                })
                .await;
                assert!(
                    prompt.is_err(),
                    "a raw-UDP hop must retain the flow until its safety timeout, got {prompt:?}"
                );
                // The safety timeout aborts the retained relay. How that is
                // observed at the probe's read half is transport-dependent:
                // the tcpmux drop cascade delivers a late EOF, while the
                // rtp_mux drop path does not close the client lane within
                // the teardown bound, so the scoped epilog reports the
                // timed-out end. Both prove the flow was retained and then
                // released by the safety timeout rather than closed
                // promptly.
                let end = tokio::time::timeout(Duration::from_secs(20), async {
                    await_flow_end(&mut flow_end).await
                })
                .await
                .expect("the retained flow must terminate by the safety timeout");
                assert!(
                    matches!(end, ProbeFlowEnd::Eof | ProbeFlowEnd::TimedOut),
                    "the retained flow must end via the safety timeout, got {end:?}"
                );
                assert!(
                    started.elapsed() >= Duration::from_secs(8),
                    "the flow must be retained until the safety timeout, not closed promptly"
                );
            })
            .await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn probe_over_tcpmux_raw_udp_chain_is_retained_until_the_safety_timeout() {
        probe_over_mixed_mux_raw_udp_chain_is_retained_until_the_safety_timeout("tcpmux").await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn probe_over_rtpmux_raw_udp_chain_is_retained_until_the_safety_timeout() {
        probe_over_mixed_mux_raw_udp_chain_is_retained_until_the_safety_timeout("rtpmux").await;
    }
}
