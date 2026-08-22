#[cfg(test)]
mod tests {
    use std::{
        io,
        sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        },
        time::Duration,
    };

    use ae::anti_replay::ReplayValidator;
    use common::{
        anti_replay::{VALIDATOR_CAPACITY, VALIDATOR_TIME_FRAME},
        connect::{ConnectorConfig, ConnectorResetSignal, connector_config_cell},
        loading::{self, ReloadableHandler, Serve},
        notify::Notify,
        proxy_runtime::{
            addr::RouteAddr,
            client::stream::{establish, probe_rtt},
            conn::stream::ConnAndAddr,
            conn_handler::{SpeedLimit, stream::StreamProxyConnHandler, udp::UdpProxyConnHandler},
            connect::udp::UdpConnector,
            context::StreamRuntime,
        },
        route::HopConfig,
        stream_runtime::{
            IoConnection, StreamServerHandleConn,
            pool::{StreamConnPool, connect_with_pool},
        },
    };
    use protocol::stream_proto::{
        addr::ConcreteStreamType,
        connect::build_concrete_stream_connector_table,
        streams::{
            kcp::build_kcp_proxy_server,
            mptcp::build_mptcp_proxy_server,
            mux::{MuxProxyConnHandler, MuxProxyHandler},
            rtp::build_rtp_proxy_server,
            rtp_mux::build_rtp_mux_proxy_server,
            tcp::proxy_server::build_tcp_proxy_server,
            tcp_mux::{TcpMuxServer, build_tcp_mux_proxy_server},
        },
    };
    use serial_test::serial;
    use swap::Swap;
    use tokio::{
        io::{AsyncRead, AsyncReadExt, AsyncWriteExt},
        net::TcpListener,
    };

    use crate::{STRESS_CHAINS, STRESS_PARALLEL, STRESS_SERIAL, scope::TestRuntimeScope};

    fn create_random_crypto() -> tokio_chacha20::config::Config {
        let key: [u8; 32] = rand::random();
        tokio_chacha20::config::Config::new(key.into())
    }

    fn stream_context(scope: &mut TestRuntimeScope) -> StreamRuntime {
        let connector_reset = ConnectorResetSignal(Notify::new());
        // Test-owned connector-driver reaper. The connector drivers run the
        // per-protocol connector loops (e.g. rtp_mux, tcp_mux) and are reaped
        // here for the lifetime of the test runtime.
        let mut connector_drivers = tokio::task::JoinSet::new();
        let connector_config = connector_config_cell(ConnectorConfig::default()).0;
        let udp_connector = UdpConnector::new(connector_config.clone());
        let connector_table = Arc::new(build_concrete_stream_connector_table(
            connector_config,
            connector_reset,
            &mut connector_drivers,
            &udp_connector,
        ));
        scope.spawn_required(async move {
            while let Some(result) = connector_drivers.join_next().await {
                // Unwrap both the `JoinError` and the driver's own result: a
                // dead connector driver must fail the test.
                result
                    .expect("connector driver panicked")
                    .expect("connector driver failed");
            }
            Ok(())
        });
        StreamRuntime {
            session_table: None,
            pool: Swap::new(StreamConnPool::empty()),
            connector_table,
            replay_validator: Arc::new(ReplayValidator::new(
                VALIDATOR_TIME_FRAME,
                VALIDATOR_CAPACITY,
            )),
            session_spawner: scope.session_spawner(),
            retention: scope.retention(),
        }
    }

    async fn spawn_proxy(
        scope: &mut TestRuntimeScope,
        addr: &Arc<str>,
        ty: ConcreteStreamType,
    ) -> HopConfig {
        spawn_proxy_(scope, addr, ty, true, false).await
    }

    async fn spawn_encrypted_proxy(
        scope: &mut TestRuntimeScope,
        addr: &Arc<str>,
        ty: ConcreteStreamType,
    ) -> HopConfig {
        spawn_proxy_(scope, addr, ty, true, true).await
    }

    async fn spawn_guarded_proxy(
        scope: &mut TestRuntimeScope,
        addr: &Arc<str>,
        ty: ConcreteStreamType,
    ) -> HopConfig {
        spawn_proxy_(scope, addr, ty, false, false).await
    }

    async fn spawn_proxy_(
        scope: &mut TestRuntimeScope,
        addr: &Arc<str>,
        ty: ConcreteStreamType,
        allow_loopback: bool,
        encrypt_payload: bool,
    ) -> HopConfig {
        let crypto = create_random_crypto();
        let payload_crypto = encrypt_payload.then(create_random_crypto);
        let stream_context = stream_context(scope);
        let session_spawner = stream_context.session_spawner.clone();
        let proxy = StreamProxyConnHandler::new(
            crypto.clone(),
            payload_crypto.clone(),
            stream_context,
            Arc::clone(addr),
            allow_loopback,
            SpeedLimit::UNLIMITED,
        );
        let proxy_addr = match ty {
            ConcreteStreamType::Tcp => {
                let server = build_tcp_proxy_server(addr.as_ref(), proxy, session_spawner.clone())
                    .await
                    .unwrap();
                let proxy_addr = server.listener().local_addr().unwrap();
                let (set_conn_handler_tx, set_conn_handler_rx) =
                    loading::replace_conn_handler_channel();
                scope.spawn_required(async move {
                    let _set_conn_handler_tx = set_conn_handler_tx;
                    server.serve(set_conn_handler_rx).await
                });
                proxy_addr
            }
            ConcreteStreamType::TcpMux => {
                let server = build_tcp_mux_proxy_server(
                    addr.as_ref(),
                    MuxProxyHandler {
                        stream: proxy,
                        udp: None,
                    },
                    session_spawner.clone(),
                )
                .await
                .unwrap();
                let proxy_addr = server.listener().local_addr().unwrap();
                let (set_conn_handler_tx, set_conn_handler_rx) =
                    loading::replace_conn_handler_channel();
                scope.spawn_required(async move {
                    let _set_conn_handler_tx = set_conn_handler_tx;
                    server.serve(set_conn_handler_rx).await
                });
                proxy_addr
            }
            ConcreteStreamType::Kcp => {
                let server = build_kcp_proxy_server(addr.as_ref(), proxy, session_spawner.clone())
                    .await
                    .unwrap();
                let proxy_addr = server.listener().local_addr().unwrap();
                let (set_conn_handler_tx, set_conn_handler_rx) =
                    loading::replace_conn_handler_channel();
                scope.spawn_required(async move {
                    let _set_conn_handler_tx = set_conn_handler_tx;
                    server.serve(set_conn_handler_rx).await
                });
                proxy_addr
            }
            ConcreteStreamType::Mptcp => {
                let server =
                    build_mptcp_proxy_server(addr.as_ref(), proxy, session_spawner.clone())
                        .await
                        .unwrap();
                let proxy_addr = server.listener().local_addrs().next().unwrap().unwrap();
                let (set_conn_handler_tx, set_conn_handler_rx) =
                    loading::replace_conn_handler_channel();
                scope.spawn_required(async move {
                    let _set_conn_handler_tx = set_conn_handler_tx;
                    server.serve(set_conn_handler_rx).await
                });
                proxy_addr
            }
            ConcreteStreamType::Rtp => {
                let server = build_rtp_proxy_server(addr.as_ref(), proxy, session_spawner.clone())
                    .await
                    .unwrap();
                let proxy_addr = server.listener().local_addr();
                let (set_conn_handler_tx, set_conn_handler_rx) =
                    loading::replace_conn_handler_channel();
                scope.spawn_required(async move {
                    let _set_conn_handler_tx = set_conn_handler_tx;
                    server.serve(set_conn_handler_rx).await
                });
                proxy_addr
            }
            ConcreteStreamType::RtpMux => {
                let server = build_rtp_mux_proxy_server(
                    addr.as_ref(),
                    MuxProxyHandler {
                        stream: proxy,
                        udp: None,
                    },
                    session_spawner.clone(),
                )
                .await
                .unwrap();
                let proxy_addr = server.listener().local_addr();
                let (set_conn_handler_tx, set_conn_handler_rx) =
                    loading::replace_conn_handler_channel();
                scope.spawn_required(async move {
                    let _set_conn_handler_tx = set_conn_handler_tx;
                    server.serve(set_conn_handler_rx).await
                });
                proxy_addr
            }
        };
        HopConfig {
            address: RouteAddr {
                address: proxy_addr.into(),
                protocol: ty.to_string().into(),
            },
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
    ) -> RouteAddr {
        let listener = TcpListener::bind(addr).await.unwrap();
        let greet_addr = listener.local_addr().unwrap();
        let req = req.to_vec();
        let resp = resp.to_vec();
        scope.spawn_session(async move {
            let mut join_set = tokio::task::JoinSet::new();
            let mut accepted = 0;
            while accepted < accepts {
                // Race accepting against the in-flight handlers so a panicked
                // handler surfaces immediately instead of being parked until
                // all accepts are done. Only the accept arm counts towards
                // `accepts`; handler completions never skip a connection.
                tokio::select! {
                    accepted_conn = listener.accept() => {
                        let (mut stream, _) = accepted_conn.unwrap();
                        accepted += 1;
                        let req = req.to_vec();
                        let resp = resp.to_vec();
                        join_set.spawn(async move {
                            let mut buf = [0; 1024];
                            let msg_buf = &mut buf[..req.len()];
                            stream.read_exact(msg_buf).await.unwrap();
                            assert_eq!(msg_buf, req);
                            stream.write_all(&resp).await.unwrap();
                        });
                    }
                    Some(result) = join_set.join_next() => {
                        result.unwrap();
                    }
                }
            }
            // All accepts done; drain the remaining handlers.
            while let Some(result) = join_set.join_next().await {
                result.unwrap();
            }
        });
        RouteAddr {
            address: greet_addr.into(),
            protocol: ConcreteStreamType::Tcp.to_string().into(),
        }
    }

    async fn read_response<Stream>(stream: &mut Stream, resp_msg: &[u8]) -> io::Result<()>
    where
        Stream: AsyncRead + Unpin,
    {
        let mut buf = [0; 1024];
        let msg_buf = &mut buf[..resp_msg.len()];
        tokio::time::timeout(Duration::from_secs(10), stream.read_exact(msg_buf))
            .await
            .expect("timed out waiting for the proxy response")
            .unwrap();
        assert_eq!(msg_buf, resp_msg);
        Ok(())
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_proxies() {
        let mut scope = TestRuntimeScope::new();
        let stream_context = stream_context(&mut scope);

        // Start proxy servers
        let addr = Arc::from("0.0.0.0:0");
        let proxy_1_config = spawn_proxy(&mut scope, &addr, ConcreteStreamType::Tcp).await;
        let proxy_2_config = spawn_proxy(&mut scope, &addr, ConcreteStreamType::Tcp).await;
        let proxy_3_config = spawn_proxy(&mut scope, &addr, ConcreteStreamType::Tcp).await;
        let proxies = vec![proxy_1_config, proxy_2_config, proxy_3_config];

        // Message to send
        let req_msg = b"hello world";
        let resp_msg = b"goodbye world";

        // Start greet server
        let greet_addr = spawn_greet(&mut scope, "[::]:0", req_msg, resp_msg, 1).await;

        scope
            .run(async {
                // Connect to proxy server
                let ConnAndAddr { mut stream, .. } = tokio::time::timeout(
                    Duration::from_secs(30),
                    establish(&proxies, greet_addr, &stream_context),
                )
                .await
                .expect("timed out establishing the proxy session")
                .unwrap();

                // Send message
                stream.write_all(req_msg).await.unwrap();

                // Read response
                read_response(&mut stream, resp_msg).await.unwrap();

                // Trace
                let rtt = tokio::time::timeout(
                    Duration::from_secs(30),
                    probe_rtt(&proxies, &stream_context),
                )
                .await
                .expect("timed out probing the proxy chain")
                .unwrap();
                assert!(rtt > Duration::from_secs(0));
                assert!(rtt < Duration::from_secs(1));
            })
            .await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn multiple_payload_keys_layer_each_stream_hop() {
        let mut scope = TestRuntimeScope::new();
        let stream_context = stream_context(&mut scope);
        let addr = Arc::from("0.0.0.0:0");
        let proxies = vec![
            spawn_encrypted_proxy(&mut scope, &addr, ConcreteStreamType::Tcp).await,
            spawn_encrypted_proxy(&mut scope, &addr, ConcreteStreamType::Tcp).await,
            spawn_encrypted_proxy(&mut scope, &addr, ConcreteStreamType::Tcp).await,
        ];
        let request = b"hello through three encrypted hops";
        let response = b"goodbye through three encrypted hops";
        let destination = spawn_greet(&mut scope, "[::]:0", request, response, 1).await;
        scope
            .run(async {
                let ConnAndAddr { mut stream, .. } = tokio::time::timeout(
                    Duration::from_secs(30),
                    establish(&proxies, destination, &stream_context),
                )
                .await
                .expect("timed out establishing the encrypted proxy chain")
                .unwrap();
                stream.write_all(request).await.unwrap();
                read_response(&mut stream, response).await.unwrap();
                let rtt = tokio::time::timeout(
                    Duration::from_secs(30),
                    probe_rtt(&proxies, &stream_context),
                )
                .await
                .expect("timed out probing the encrypted proxy chain")
                .unwrap();
                assert!(rtt > Duration::ZERO);
                assert!(rtt < Duration::from_secs(1));
            })
            .await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_clients() {
        let mut scope = TestRuntimeScope::new();
        let stream_context = stream_context(&mut scope);

        // Start proxy servers
        let addr = Arc::from("0.0.0.0:0");
        let proxy_1_config = spawn_proxy(&mut scope, &addr, ConcreteStreamType::Tcp).await;
        let proxy_2_config = spawn_proxy(&mut scope, &addr, ConcreteStreamType::Tcp).await;
        let proxies = vec![proxy_1_config, proxy_2_config];

        // Message to send
        let req_msg = b"hello world";
        let resp_msg = b"goodbye world";

        let clients = 2;

        // Start greet server
        let greet_addr = spawn_greet(&mut scope, "[::]:0", req_msg, resp_msg, clients).await;

        scope
            .run(async {
                let mut handles = tokio::task::JoinSet::new();

                for _ in 0..clients {
                    let proxies = proxies.clone();
                    let greet_addr = greet_addr.clone();
                    let stream_context = stream_context.clone();
                    handles.spawn(async move {
                        // Connect to proxy server
                        let ConnAndAddr { mut stream, .. } = tokio::time::timeout(
                            Duration::from_secs(30),
                            establish(&proxies, greet_addr, &stream_context),
                        )
                        .await
                        .expect("timed out establishing the proxy session")
                        .unwrap();

                        // Send message
                        stream.write_all(req_msg).await.unwrap();

                        // Read response
                        read_response(&mut stream, resp_msg).await.unwrap();
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
    async fn stress_test_tcp() {
        stress_test(ConcreteStreamType::Tcp).await
    }

    #[tokio::test(flavor = "multi_thread")]
    #[serial]
    #[ignore = "kcp stress is flaky in CI; run manually on demand"]
    async fn stress_test_kcp() {
        stress_test(ConcreteStreamType::Kcp).await
    }

    #[tokio::test(flavor = "multi_thread")]
    #[serial]
    async fn stress_test_mptcp() {
        stress_test(ConcreteStreamType::Mptcp).await
    }

    #[tokio::test(flavor = "multi_thread")]
    #[serial]
    async fn stress_test_rtp() {
        stress_test(ConcreteStreamType::Rtp).await
    }

    #[tokio::test(flavor = "multi_thread")]
    #[serial]
    async fn stress_test_rtp_mux() {
        stress_test(ConcreteStreamType::RtpMux).await
    }

    async fn stress_test(ty: ConcreteStreamType) {
        let mut scope = TestRuntimeScope::new();
        let stream_context = stream_context(&mut scope);

        // Start proxy servers
        let mut proxies = Vec::new();
        let addr = Arc::from("0.0.0.0:0");
        for _ in 0..STRESS_CHAINS {
            let proxy_config = spawn_proxy(&mut scope, &addr, ty).await;
            proxies.push(proxy_config);
        }

        // Message to send
        let req_msg = b"hello world";
        let resp_msg = b"goodbye world";

        // Start greet server
        let greet_addr = spawn_greet(&mut scope, "[::]:0", req_msg, resp_msg, usize::MAX).await;

        scope
            .run(async {
                let mut handles = tokio::task::JoinSet::new();

                for _ in 0..STRESS_PARALLEL {
                    let proxies = proxies.clone();
                    let greet_addr = greet_addr.clone();
                    let stream_context = stream_context.clone();
                    handles.spawn(async move {
                        for _ in 0..STRESS_SERIAL {
                            let greet_addr = greet_addr.clone();
                            // Connect to proxy server
                            let ConnAndAddr { mut stream, .. } = tokio::time::timeout(
                                Duration::from_secs(30),
                                establish(&proxies, greet_addr, &stream_context),
                            )
                            .await
                            .expect("timed out establishing the proxy session")
                            .unwrap();

                            // Send message
                            stream.write_all(req_msg).await.unwrap();

                            // Read response
                            read_response(&mut stream, resp_msg).await.unwrap();
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
    #[serial]
    #[ignore = "performance benchmark; not part of the default test run"]
    async fn perf_bulk_rtp_mux() {
        use std::time::Instant;

        let mut scope = TestRuntimeScope::new();
        let stream_context = stream_context(&mut scope);

        // Start proxy servers
        let mut proxies = Vec::new();
        let addr = Arc::from("0.0.0.0:0");
        for _ in 0..STRESS_CHAINS {
            let proxy_config = spawn_proxy(&mut scope, &addr, ConcreteStreamType::RtpMux).await;
            proxies.push(proxy_config);
        }

        // Local TCP receiver: reads exactly TOTAL_BYTES then sends 1-byte ack
        const TOTAL_BYTES: usize = 32 * 1024 * 1024;
        const CHUNK: usize = 64 * 1024;

        let listener = TcpListener::bind("[::]:0").await.unwrap();
        let receiver_addr = listener.local_addr().unwrap();
        let receiver_greet_addr = RouteAddr {
            address: receiver_addr.into(),
            protocol: ConcreteStreamType::Tcp.to_string().into(),
        };

        scope.spawn_session(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut got = 0usize;
            let mut buf = [0u8; CHUNK];
            while got < TOTAL_BYTES {
                let want = std::cmp::min(CHUNK, TOTAL_BYTES - got);
                let n = stream.read_exact(&mut buf[..want]).await.unwrap();
                debug_assert_eq!(n, want);
                got += n;
            }
            assert_eq!(got, TOTAL_BYTES);
            stream.write_all(&[0u8]).await.unwrap();
        });

        scope
            .run(async {
                // Establish a single stream through the proxy chain
                let ConnAndAddr { mut stream, .. } = tokio::time::timeout(
                    Duration::from_secs(30),
                    establish(&proxies, receiver_greet_addr, &stream_context),
                )
                .await
                .expect("timed out establishing the proxy session")
                .unwrap();

                // Send TOTAL_BYTES in CHUNK-sized chunks
                let chunk = vec![0u8; CHUNK];
                let start = Instant::now();
                let mut sent = 0usize;
                while sent < TOTAL_BYTES {
                    let want = std::cmp::min(CHUNK, TOTAL_BYTES - sent);
                    stream.write_all(&chunk[..want]).await.unwrap();
                    sent += want;
                }
                // Wait for the 1-byte ack from the receiver
                let mut ack = [0u8; 1];
                stream.read_exact(&mut ack).await.unwrap();
                let elapsed = start.elapsed();

                let mib = (TOTAL_BYTES as f64) / (1024.0 * 1024.0);
                let secs = elapsed.as_secs_f64();
                let mib_s = mib / secs;
                println!("perf_bulk_rtp_mux_mib_s={mib_s:.3}");
            })
            .await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_no_proxies() {
        let mut scope = TestRuntimeScope::new();

        let req_msg = b"hello world";
        let resp_msg = b"goodbye world";

        let greet_addr = spawn_greet(&mut scope, "[::]:0", req_msg, resp_msg, 1).await;

        let stream_context = stream_context(&mut scope);
        scope
            .run(async {
                let ConnAndAddr { mut stream, .. } = tokio::time::timeout(
                    Duration::from_secs(30),
                    establish(&[], greet_addr, &stream_context),
                )
                .await
                .expect("timed out establishing the proxy session")
                .unwrap();

                stream.write_all(req_msg).await.unwrap();
                read_response(&mut stream, resp_msg).await.unwrap();
            })
            .await;
    }

    async fn assert_refused(
        stream_context: &StreamRuntime,
        proxies: &[HopConfig],
        greet_addr: RouteAddr,
    ) {
        let mut stream = match tokio::time::timeout(
            Duration::from_secs(30),
            establish(proxies, greet_addr, stream_context),
        )
        .await
        .expect("timed out establishing the proxy session")
        {
            Ok(ConnAndAddr { stream, .. }) => stream,
            Err(_) => {
                // The guarded proxy dropped the connection during the
                // handshake; the loopback destination was never reached.
                return;
            }
        };
        let _ = stream.write_all(b"hello world").await;
        let mut buf = [0u8; 1024];
        match stream.read(&mut buf).await {
            Ok(0) => {}
            Ok(_) => panic!("the guarded proxy relayed to a loopback/unspecified service"),
            Err(_) => {
                // Connection reset / broken pipe: the guarded proxy dropped us
                // before relaying to the loopback destination.
            }
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_bad_proxy() {
        let mut scope = TestRuntimeScope::new();
        let stream_context = stream_context(&mut scope);
        let addr = Arc::from("0.0.0.0:0");
        let proxy_1_config = spawn_guarded_proxy(&mut scope, &addr, ConcreteStreamType::Tcp).await;
        let proxy_2_config = spawn_guarded_proxy(&mut scope, &addr, ConcreteStreamType::Tcp).await;
        let proxy_3_config = spawn_guarded_proxy(&mut scope, &addr, ConcreteStreamType::Tcp).await;
        let req_msg = b"hello world";
        let resp_msg = b"goodbye world";
        let greet_addr = spawn_greet(&mut scope, "[::]:0", req_msg, resp_msg, 1).await;
        scope
            .run(async {
                assert_refused(
                    &stream_context,
                    &[proxy_1_config, proxy_2_config, proxy_3_config],
                    greet_addr,
                )
                .await;
            })
            .await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn loopback_spelled_as_ipv6_is_still_refused() {
        let mut scope = TestRuntimeScope::new();
        let stream_context = stream_context(&mut scope);
        let addr = Arc::from("0.0.0.0:0");
        let proxy_config = spawn_guarded_proxy(&mut scope, &addr, ConcreteStreamType::Tcp).await;
        let req_msg = b"hello world";
        let resp_msg = b"goodbye world";
        let greet_addr = spawn_greet(&mut scope, "127.0.0.1:0", req_msg, resp_msg, 1).await;
        let greet_port = greet_addr.address.port();
        let mapped: RouteAddr = RouteAddr {
            address: std::net::SocketAddr::new(
                std::net::Ipv4Addr::new(127, 0, 0, 1)
                    .to_ipv6_mapped()
                    .into(),
                greet_port,
            )
            .into(),
            protocol: ConcreteStreamType::Tcp.to_string().into(),
        };
        scope
            .run(async {
                assert_refused(&stream_context, &[proxy_config], mapped).await;
            })
            .await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn an_unspecified_destination_is_still_refused() {
        let mut scope = TestRuntimeScope::new();
        let stream_context = stream_context(&mut scope);
        let addr = Arc::from("0.0.0.0:0");
        let proxy_config = spawn_guarded_proxy(&mut scope, &addr, ConcreteStreamType::Tcp).await;
        let req_msg = b"hello world";
        let resp_msg = b"goodbye world";
        let greet_addr = spawn_greet(&mut scope, "0.0.0.0:0", req_msg, resp_msg, 1).await;
        let greet_port = greet_addr.address.port();
        let unspecified: RouteAddr = RouteAddr {
            address: std::net::SocketAddr::new(std::net::Ipv4Addr::UNSPECIFIED.into(), greet_port)
                .into(),
            protocol: ConcreteStreamType::Tcp.to_string().into(),
        };
        scope
            .run(async {
                assert_refused(&stream_context, &[proxy_config], unspecified).await;
            })
            .await;
    }

    #[tokio::test(flavor = "multi_thread")]
    #[serial]
    async fn test_rtp_mux_migration_integrity() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let mut scope = TestRuntimeScope::new();
        let stream_context = stream_context(&mut scope);

        // Start proxy server
        let addr = Arc::from("0.0.0.0:0");
        let proxy_config = spawn_proxy(&mut scope, &addr, ConcreteStreamType::RtpMux).await;
        let proxies = vec![proxy_config];

        // Start an echo server that reads a 4-byte length prefix then echoes
        // back exactly that many bytes. This lets us test both directions
        // independently.
        let listener = TcpListener::bind("[::]:0").await.unwrap();
        let echo_addr = listener.local_addr().unwrap();
        let greet_addr = RouteAddr {
            address: echo_addr.into(),
            protocol: ConcreteStreamType::Tcp.to_string().into(),
        };
        scope.spawn_session(async move {
            let mut handlers = tokio::task::JoinSet::new();
            loop {
                // Race accepting against the in-flight handlers so a panicked
                // handler surfaces immediately instead of being parked until
                // the listener fails.
                tokio::select! {
                    accepted = listener.accept() => {
                        let (mut sock, _) = match accepted {
                            Ok(x) => x,
                            Err(_) => break,
                        };
                        handlers.spawn(async move {
                            loop {
                                let mut len_buf = [0u8; 4];
                                if sock.read_exact(&mut len_buf).await.is_err() {
                                    return;
                                }
                                let len = u32::from_be_bytes(len_buf) as usize;
                                let len = std::cmp::min(len, 1024 * 1024);
                                let mut data = vec![0u8; len];
                                if sock.read_exact(&mut data).await.is_err() {
                                    return;
                                }
                                let _ = sock.write_all(&len_buf).await;
                                let _ = sock.write_all(&data).await;
                            }
                        });
                    }
                    Some(result) = handlers.join_next() => {
                        result.unwrap();
                    }
                }
            }
            // The listener failed or was dropped; drain the remaining
            // handlers.
            while let Some(result) = handlers.join_next().await {
                result.unwrap();
            }
        });

        let concurrent = 4;
        scope
            .run(async {
                let mut handles = tokio::task::JoinSet::new();

                for stream_idx in 0..concurrent {
                    let proxies = proxies.clone();
                    let greet_addr = greet_addr.clone();
                    let stream_context = stream_context.clone();
                    handles.spawn(async move {
                        let ConnAndAddr { mut stream, .. } = tokio::time::timeout(
                            Duration::from_secs(30),
                            establish(&proxies, greet_addr, &stream_context),
                        )
                        .await
                        .expect("timed out establishing the proxy session")
                        .unwrap();

                        // Large burst >2048 → bulk lane
                        let large: Vec<u8> = (0..4096u16)
                            .map(|i| ((i + stream_idx as u16) % 256) as u8)
                            .collect();
                        let len = large.len() as u32;
                        stream.write_all(&len.to_be_bytes()).await.unwrap();
                        stream.write_all(&large).await.unwrap();

                        // Read echo
                        let mut echo_len_buf = [0u8; 4];
                        stream.read_exact(&mut echo_len_buf).await.unwrap();
                        assert_eq!(u32::from_be_bytes(echo_len_buf), len);
                        let mut echo = vec![0u8; large.len()];
                        stream.read_exact(&mut echo).await.unwrap();
                        assert_eq!(echo, large, "large echo mismatch stream {stream_idx}");

                        // Many small writes → interactive lane (after demotion)
                        for i in 0..20u8 {
                            let small: Vec<u8> = vec![i; 64];
                            let slen = small.len() as u32;
                            stream.write_all(&slen.to_be_bytes()).await.unwrap();
                            stream.write_all(&small).await.unwrap();

                            let mut sel_buf = [0u8; 4];
                            stream.read_exact(&mut sel_buf).await.unwrap();
                            assert_eq!(u32::from_be_bytes(sel_buf), slen);
                            let mut small_echo = vec![0u8; 64];
                            stream.read_exact(&mut small_echo).await.unwrap();
                            assert_eq!(
                                small_echo, small,
                                "small echo mismatch stream {stream_idx} iter {i}"
                            );
                        }
                    });
                }

                while let Some(x) = handles.join_next().await {
                    x.unwrap();
                }
            })
            .await;
    }

    /// A stream handler that counts how many streams it served and echoes a
    /// single byte back, so a test can tell which handler generation served
    /// a given substream.
    #[derive(Debug)]
    struct EchoCountingHandler {
        served: Arc<AtomicUsize>,
    }
    impl loading::HandleConn for EchoCountingHandler {}
    impl StreamServerHandleConn for EchoCountingHandler {
        async fn handle_stream<Stream>(&self, mut stream: Stream)
        where
            Stream: IoConnection + std::fmt::Debug,
        {
            self.served.fetch_add(1, Ordering::SeqCst);
            let mut byte = [0u8; 1];
            if stream.read_exact(&mut byte).await.is_ok() {
                let _ = stream.write_all(&byte).await;
            }
        }
    }
    impl MuxProxyConnHandler for EchoCountingHandler {
        fn udp_proxy(&self) -> Option<&UdpProxyConnHandler> {
            None
        }
    }

    /// A handler reload must reach substreams opened on TCP-mux sessions
    /// that predate the reload: each TCP connection's mux accepter serves
    /// every substream with the *current* handler, not the one captured at
    /// TCP-accept time.
    #[tokio::test(flavor = "multi_thread")]
    async fn tcp_mux_reload_reaches_existing_session_substreams() {
        let mut scope = TestRuntimeScope::new();
        let stream_context = stream_context(&mut scope);

        // Two handler generations distinguishable only by the counter they
        // bump.
        let served_old = Arc::new(AtomicUsize::new(0));
        let served_new = Arc::new(AtomicUsize::new(0));
        let handler_old = EchoCountingHandler {
            served: Arc::clone(&served_old),
        };
        let handler_new = EchoCountingHandler {
            served: Arc::clone(&served_new),
        };

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let proxy_addr = listener.local_addr().unwrap();
        // The reloadable handler cell doubles as the reload acknowledgement:
        // its generation watch bumps only once the serve_loop has installed
        // the replacement, so the test can wait deterministically.
        let reloadable = ReloadableHandler::new(handler_old);
        let mut generation = reloadable.generation();
        let server = TcpMuxServer::with_reloadable(
            listener,
            reloadable,
            stream_context.session_spawner.clone(),
        );
        let (set_conn_handler_tx, set_conn_handler_rx) = loading::replace_conn_handler_channel();
        let set_conn_handler_tx_for_server = set_conn_handler_tx.clone();
        scope.spawn_required(async move {
            // Keep the sender alive so the server's receiver stays open;
            // the test holds its own clone for the reload.
            let _set_conn_handler_tx = set_conn_handler_tx_for_server;
            server.serve(set_conn_handler_rx).await
        });

        // Client route straight to the tcp_mux proxy (no relay headers: the
        // echo handler just echoes one byte). The tcp_mux connector keeps
        // the per-address mux session open between dials.
        let proxy_route = RouteAddr {
            address: proxy_addr.into(),
            protocol: "tcpmux".into(),
        };
        let dial = || async {
            connect_with_pool(&proxy_route, &stream_context, true, Duration::from_secs(10))
                .await
                .unwrap()
                .0
        };
        let round_trip = |byte: u8| async move {
            let mut stream = dial().await;
            stream.write_all(&[byte]).await.unwrap();
            let mut echoed = [0u8; 1];
            tokio::time::timeout(Duration::from_secs(10), stream.read_exact(&mut echoed))
                .await
                .expect("timed out waiting for the handler echo")
                .unwrap();
            assert_eq!(echoed[0], byte, "echo mismatch");
        };

        scope
            .run(async {
                // First substream: served by the original handler generation.
                round_trip(b'A').await;
                assert_eq!(served_old.load(Ordering::SeqCst), 1);
                assert_eq!(served_new.load(Ordering::SeqCst), 0);

                // Reload the handler while the TCP mux session stays open,
                // and wait for the generation bump — the serve_loop replaces
                // the handler in the shared cell before bumping, so the next
                // substream is guaranteed to be dispatched with the reloaded
                // generation, regardless of scheduler timing.
                set_conn_handler_tx.send(handler_new).unwrap();
                tokio::time::timeout(Duration::from_secs(10), generation.changed())
                    .await
                    .expect("timed out waiting for the handler reload to be installed")
                    .expect("reload generation watch closed");

                // Second substream on the same session: must be served by the
                // reloaded handler, not the one pinned at TCP-accept time.
                round_trip(b'B').await;
                assert_eq!(
                    served_old.load(Ordering::SeqCst),
                    1,
                    "pre-reload handler must not serve post-reload substreams"
                );
                assert_eq!(
                    served_new.load(Ordering::SeqCst),
                    1,
                    "reloaded handler must serve substreams on existing sessions"
                );
            })
            .await;
    }
}
