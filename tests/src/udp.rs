#[cfg(test)]
mod tests {
    use std::{
        sync::{Arc, RwLock},
        time::Duration,
    };

    use ae::anti_replay::{ReplayValidator, TimeValidator};
    use bytes::BytesMut;
    use common::{
        addr::InternetAddr,
        anti_replay::{VALIDATOR_CAPACITY, VALIDATOR_TIME_FRAME, VALIDATOR_UDP_HDR_TTL},
        connect::{ConnectorConfig, ConnectorResetSignal},
        header::route::RouteErrorKind,
        loading::{self, Serve},
        notify::Notify,
        proto::{
            addr::RouteAddr,
            client::{
                self,
                udp::{UdpProxyClient, UdpProxyClientReadHalf, probe_rtt},
            },
            conn_handler::{stream::StreamProxyConnHandler, udp::UdpProxyConnHandler},
            connect::udp::UdpConnector,
            context::{Runtime, StreamRuntime, UdpRuntime},
        },
        route::ConnConfig,
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

    use crate::{STRESS_CHAINS, STRESS_PARALLEL, STRESS_SERIAL};

    fn create_random_crypto() -> tokio_chacha20::config::Config {
        let key: [u8; 32] = rand::random();
        tokio_chacha20::config::Config::new(key.into())
    }

    fn udp_context(join_set: &mut tokio::task::JoinSet<()>) -> UdpRuntime {
        let (session_spawner, mut session_rx) = common::session::SessionSpawner::channel();
        join_set.spawn(async move {
            let mut sessions = tokio::task::JoinSet::new();
            loop {
                tokio::select! {
                    Some(fut) = session_rx.recv() => { sessions.spawn(fut); }
                    Some(res) = sessions.join_next() => { let _ = res.unwrap(); }
                    else => break,
                }
            }
        });
        let (retention_actor, retention) = common::retention::RetentionActor::new();
        join_set.spawn(async move {
            let _ = retention_actor.run().await;
        });
        UdpRuntime {
            session_table: None,
            connector: Arc::new(UdpConnector::new(Arc::new(RwLock::new(
                ConnectorConfig::default(),
            )))),
            time_validator: Arc::new(TimeValidator::new(
                VALIDATOR_TIME_FRAME + VALIDATOR_UDP_HDR_TTL,
            )),
            session_spawner,
            retention,
        }
    }

    async fn spawn_proxy(join_set: &mut tokio::task::JoinSet<()>, addr: &str) -> ConnConfig {
        spawn_proxy_(join_set, addr, true, false).await
    }

    async fn spawn_encrypted_proxy(
        join_set: &mut tokio::task::JoinSet<()>,
        addr: &str,
    ) -> ConnConfig {
        spawn_proxy_(join_set, addr, true, true).await
    }

    async fn spawn_guarded_proxy(
        join_set: &mut tokio::task::JoinSet<()>,
        addr: &str,
    ) -> ConnConfig {
        spawn_proxy_(join_set, addr, false, false).await
    }

    async fn spawn_proxy_(
        join_set: &mut tokio::task::JoinSet<()>,
        addr: &str,
        allow_loopback: bool,
        encrypt_payload: bool,
    ) -> ConnConfig {
        let crypto = create_random_crypto();
        let payload_crypto = encrypt_payload.then(create_random_crypto);
        let proxy = UdpProxyConnHandler::new(
            crypto.clone(),
            payload_crypto.clone(),
            udp_context(join_set),
            allow_loopback,
        );
        let server = proxy.build(addr).await.unwrap();
        let proxy_addr = server.listener().local_addr().unwrap();
        join_set.spawn(async move {
            let (_set_conn_handler_tx, set_conn_handler_rx) =
                loading::replace_conn_handler_channel();
            server.serve(set_conn_handler_rx).await.unwrap();
        });
        ConnConfig {
            address: common::proto::addr::RouteAddr::udp(proxy_addr.into()),
            header_crypto: crypto,
            payload_crypto,
        }
    }

    async fn spawn_greet(
        join_set: &mut tokio::task::JoinSet<()>,
        addr: &str,
        req: &[u8],
        resp: &[u8],
        accepts: usize,
    ) -> InternetAddr {
        let listener = UdpSocket::bind(addr).await.unwrap();
        let greet_addr = listener.local_addr().unwrap();
        let req = req.to_vec();
        let resp = resp.to_vec();
        join_set.spawn(async move {
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
        let mut join_set = tokio::task::JoinSet::new();
        let context = udp_context(&mut join_set);

        let mut pkt_buf = BytesMut::with_capacity(PACKET_BUFFER_LENGTH);

        // Start proxy servers
        let proxy_1_config = spawn_proxy(&mut join_set, "127.0.0.1:0").await;
        let proxy_2_config = spawn_proxy(&mut join_set, "127.0.0.1:0").await;
        let proxy_3_config = spawn_proxy(&mut join_set, "127.0.0.1:0").await;
        let proxies: Arc<[_]> = vec![proxy_1_config, proxy_2_config, proxy_3_config].into();

        // Message to send
        let req_msg = b"hello world";
        let resp_msg = b"goodbye world";

        // Start greet server
        let greet_addr = spawn_greet(&mut join_set, "127.0.0.1:0", req_msg, resp_msg, 1).await;

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
        .unwrap();
        assert!(rtt > Duration::from_secs(0));
        assert!(rtt < Duration::from_secs(1));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn multiple_payload_keys_layer_each_udp_hop() {
        let mut join_set = tokio::task::JoinSet::new();
        let context = udp_context(&mut join_set);
        let proxies: Arc<[_]> = vec![
            spawn_encrypted_proxy(&mut join_set, "127.0.0.1:0").await,
            spawn_encrypted_proxy(&mut join_set, "127.0.0.1:0").await,
            spawn_encrypted_proxy(&mut join_set, "127.0.0.1:0").await,
        ]
        .into();
        let request = b"hello through three encrypted UDP hops";
        let response = b"goodbye through three encrypted UDP hops";
        let destination = spawn_greet(&mut join_set, "127.0.0.1:0", request, response, 1).await;
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
        .unwrap();
        assert!(rtt > Duration::ZERO);
        assert!(rtt < Duration::from_secs(1));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_clients() {
        let mut join_set = tokio::task::JoinSet::new();
        let context = udp_context(&mut join_set);

        // Start proxy servers
        let proxy_1_config = spawn_proxy(&mut join_set, "127.0.0.1:0").await;
        let proxy_2_config = spawn_proxy(&mut join_set, "127.0.0.1:0").await;
        let proxies: Arc<[_]> = vec![proxy_1_config, proxy_2_config].into();

        // Message to send
        let req_msg = b"hello world";
        let resp_msg = b"goodbye world";

        let clients = 2;

        // Start greet server
        let greet_addr =
            spawn_greet(&mut join_set, "127.0.0.1:0", req_msg, resp_msg, clients).await;

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
    }

    #[tokio::test(flavor = "multi_thread")]
    #[serial]
    async fn stress_test() {
        tokio::time::sleep(Duration::from_secs_f64(0.6)).await;

        let mut join_set = tokio::task::JoinSet::new();
        let context = udp_context(&mut join_set);

        // Start proxy servers
        let mut proxies = Vec::new();
        for _ in 0..STRESS_CHAINS {
            let proxy_config = spawn_proxy(&mut join_set, "127.0.0.1:0").await;
            proxies.push(proxy_config);
        }
        let proxies: Arc<[_]> = proxies.into();

        // Message to send
        let req_msg = b"hello world";
        let resp_msg = b"goodbye world";

        // Start greet server
        let greet_addr =
            spawn_greet(&mut join_set, "127.0.0.1:0", req_msg, resp_msg, usize::MAX).await;

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
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_bad_proxy() {
        let mut join_set = tokio::task::JoinSet::new();
        let context = udp_context(&mut join_set);
        let proxy_1_config = spawn_guarded_proxy(&mut join_set, "localhost:0").await;
        let proxy_2_config = spawn_guarded_proxy(&mut join_set, "localhost:0").await;
        let proxy_3_config = spawn_guarded_proxy(&mut join_set, "localhost:0").await;
        let req_msg = b"hello world";
        let resp_msg = b"goodbye world";
        let greet_addr = spawn_greet(&mut join_set, "127.0.0.1:0", req_msg, resp_msg, 1).await;
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
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn loopback_spelled_as_ipv6_is_still_refused() {
        let mut join_set = tokio::task::JoinSet::new();
        let context = udp_context(&mut join_set);
        let proxy_config = spawn_guarded_proxy(&mut join_set, "127.0.0.1:0").await;
        let req_msg = b"hello world";
        let resp_msg = b"goodbye world";
        let greet_addr = spawn_greet(&mut join_set, "127.0.0.1:0", req_msg, resp_msg, 1).await;
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
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn an_unspecified_destination_is_still_refused() {
        let mut join_set = tokio::task::JoinSet::new();
        let context = udp_context(&mut join_set);
        let proxy_config = spawn_guarded_proxy(&mut join_set, "127.0.0.1:0").await;
        let req_msg = b"hello world";
        let resp_msg = b"goodbye world";
        let greet_addr = spawn_greet(&mut join_set, "127.0.0.1:0", req_msg, resp_msg, 1).await;
        let unspecified: InternetAddr =
            std::net::SocketAddr::new(std::net::Ipv4Addr::UNSPECIFIED.into(), greet_addr.port())
                .into();
        let client = tokio::time::timeout(
            std::time::Duration::from_secs(30),
            UdpProxyClient::establish(vec![proxy_config.clone()].into(), unspecified, &context),
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
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_no_proxies() {
        let mut join_set = tokio::task::JoinSet::new();
        let context = udp_context(&mut join_set);

        // Start proxy servers

        // Message to send
        let req_msg = b"hello world";
        let resp_msg = b"goodbye world";

        // Start greet server
        let greet_addr = spawn_greet(&mut join_set, "127.0.0.1:0", req_msg, resp_msg, 1).await;

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
    }

    /// A full runtime whose `UdpConnector` has the tcpmux/rtpmux/rtpmuxfec
    /// mux dialers registered, so UDP proxy chains can carry datagrams over
    /// a mux stream with the reverse-tunnel wire format.
    async fn mux_udp_runtime(join_set: &mut tokio::task::JoinSet<()>) -> Runtime {
        let (session_spawner, mut session_rx) = common::session::SessionSpawner::channel();
        join_set.spawn(async move {
            let mut sessions = tokio::task::JoinSet::new();
            loop {
                tokio::select! {
                    Some(fut) = session_rx.recv() => { sessions.spawn(fut); }
                    Some(res) = sessions.join_next() => { let _ = res.unwrap(); }
                    else => break,
                }
            }
        });
        let mut connector_drivers = tokio::task::JoinSet::new();
        let udp_connector = Arc::new(UdpConnector::new(Arc::new(RwLock::new(
            ConnectorConfig::default(),
        ))));
        let connector_table = Arc::new(build_concrete_stream_connector_table(
            ConnectorConfig::default(),
            ConnectorResetSignal(Notify::new()),
            &mut connector_drivers,
            &udp_connector,
        ));
        join_set.spawn(async move {
            while let Some(res) = connector_drivers.join_next().await {
                let _ = res.unwrap();
            }
        });
        let (retention_actor, retention) = common::retention::RetentionActor::new();
        join_set.spawn(async move {
            let _ = retention_actor.run().await;
        });
        let stream = StreamRuntime {
            session_table: None,
            pool: Swap::new(StreamConnPool::empty()),
            connector_table,
            replay_validator: Arc::new(ReplayValidator::new(
                VALIDATOR_TIME_FRAME,
                VALIDATOR_CAPACITY,
            )),
            session_spawner: session_spawner.clone(),
            retention,
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
    /// datagram flows, and return the chain config pointing at it.
    async fn spawn_mux_udp_proxy(
        join_set: &mut tokio::task::JoinSet<()>,
        runtime: &Runtime,
        protocol: &str,
    ) -> ConnConfig {
        let crypto = create_random_crypto();
        let payload_crypto = create_random_crypto();
        let stream_proxy = StreamProxyConnHandler::new(
            crypto.clone(),
            Some(payload_crypto.clone()),
            runtime.stream.clone(),
            Arc::from(format!("udp-over-{protocol}")),
            true,
        );
        let udp_proxy = UdpProxyConnHandler::new(
            crypto.clone(),
            Some(payload_crypto.clone()),
            runtime.udp.clone(),
            true,
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
                join_set.spawn(async move {
                    let _set_conn_handler_tx = set_conn_handler_tx;
                    server.serve(set_conn_handler_rx).await.unwrap();
                });
                addr
            }
            "rtpmux" => {
                let server =
                    build_rtp_mux_proxy_server("127.0.0.1:0", handler, false, session_spawner)
                        .await
                        .unwrap();
                let addr = server.listener().local_addr();
                join_set.spawn(async move {
                    let _set_conn_handler_tx = set_conn_handler_tx;
                    server.serve(set_conn_handler_rx).await.unwrap();
                });
                addr
            }
            other => panic!("unsupported mux protocol {other}"),
        };
        ConnConfig {
            address: RouteAddr {
                address: proxy_addr.into(),
                protocol: protocol.into(),
            },
            header_crypto: crypto,
            payload_crypto: Some(payload_crypto),
        }
    }

    async fn verify_udp_flows_over_mux_proxy(protocol: &str) {
        let mut join_set = tokio::task::JoinSet::new();
        let runtime = mux_udp_runtime(&mut join_set).await;
        let proxy_config = spawn_mux_udp_proxy(&mut join_set, &runtime, protocol).await;
        let req_msg = b"ping";
        let resp_msg = b"pong";
        let greet_addr = spawn_greet(&mut join_set, "127.0.0.1:0", req_msg, resp_msg, 2).await;
        let client = tokio::time::timeout(
            Duration::from_secs(30),
            UdpProxyClient::establish(vec![proxy_config].into(), greet_addr, &runtime.udp),
        )
        .await
        .expect("timed out establishing the UDP session over the mux proxy")
        .unwrap();
        let (mut client_read, mut client_write) = client.into_split();
        // Two datagrams over the same mux UDP flow; the second exercises the
        // compact request form after the first response confirms the route.
        client_write.send(req_msg).await.unwrap();
        read_response(&mut client_read, resp_msg).await.unwrap();
        client_write.send(req_msg).await.unwrap();
        read_response(&mut client_read, resp_msg).await.unwrap();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn udp_datagrams_flow_over_tcpmux_proxy() {
        verify_udp_flows_over_mux_proxy("tcpmux").await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn udp_datagrams_flow_over_rtpmux_proxy() {
        verify_udp_flows_over_mux_proxy("rtpmux").await;
    }
}
