#[cfg(test)]
mod tests {
    use std::{
        sync::{Arc, RwLock},
        time::Duration,
    };

    use ae::anti_replay::TimeValidator;
    use bytes::BytesMut;
    use common::{
        addr::InternetAddr,
        anti_replay::{VALIDATOR_TIME_FRAME, VALIDATOR_UDP_HDR_TTL},
        connect::ConnectorConfig,
        header::route::RouteErrorKind,
        loading::{self, Serve},
        proto::{
            client::{
                self,
                udp::{UdpProxyClient, UdpProxyClientReadHalf, probe_rtt},
            },
            conn_handler::udp::UdpProxyConnHandler,
            connect::udp::UdpConnector,
            context::UdpRuntime,
        },
        route::ConnConfig,
        udp::PACKET_BUFFER_LENGTH,
    };
    use serial_test::serial;
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
        spawn_proxy_(join_set, addr, true).await
    }

    async fn spawn_guarded_proxy(
        join_set: &mut tokio::task::JoinSet<()>,
        addr: &str,
    ) -> ConnConfig {
        spawn_proxy_(join_set, addr, false).await
    }

    async fn spawn_proxy_(
        join_set: &mut tokio::task::JoinSet<()>,
        addr: &str,
        allow_loopback: bool,
    ) -> ConnConfig {
        let crypto = create_random_crypto();
        let proxy =
            UdpProxyConnHandler::new(crypto.clone(), None, udp_context(join_set), allow_loopback);
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
            payload_crypto: None,
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
        let proxy_1_config = spawn_proxy(&mut join_set, "0.0.0.0:0").await;
        let proxy_2_config = spawn_proxy(&mut join_set, "0.0.0.0:0").await;
        let proxy_3_config = spawn_proxy(&mut join_set, "0.0.0.0:0").await;
        let proxies: Arc<[_]> = vec![proxy_1_config, proxy_2_config, proxy_3_config].into();

        // Message to send
        let req_msg = b"hello world";
        let resp_msg = b"goodbye world";

        // Start greet server
        let greet_addr = spawn_greet(&mut join_set, "[::]:0", req_msg, resp_msg, 1).await;

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
    async fn test_clients() {
        let mut join_set = tokio::task::JoinSet::new();
        let context = udp_context(&mut join_set);

        // Start proxy servers
        let proxy_1_config = spawn_proxy(&mut join_set, "0.0.0.0:0").await;
        let proxy_2_config = spawn_proxy(&mut join_set, "0.0.0.0:0").await;
        let proxies: Arc<[_]> = vec![proxy_1_config, proxy_2_config].into();

        // Message to send
        let req_msg = b"hello world";
        let resp_msg = b"goodbye world";

        let clients = 2;

        // Start greet server
        let greet_addr = spawn_greet(&mut join_set, "[::]:0", req_msg, resp_msg, clients).await;

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
            let proxy_config = spawn_proxy(&mut join_set, "0.0.0.0:0").await;
            proxies.push(proxy_config);
        }
        let proxies: Arc<[_]> = proxies.into();

        // Message to send
        let req_msg = b"hello world";
        let resp_msg = b"goodbye world";

        // Start greet server
        let greet_addr = spawn_greet(&mut join_set, "[::]:0", req_msg, resp_msg, usize::MAX).await;

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
        let greet_addr = spawn_greet(&mut join_set, "[::]:0", req_msg, resp_msg, 1).await;
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
        let proxy_config = spawn_guarded_proxy(&mut join_set, "0.0.0.0:0").await;
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
        let proxy_config = spawn_guarded_proxy(&mut join_set, "0.0.0.0:0").await;
        let req_msg = b"hello world";
        let resp_msg = b"goodbye world";
        let greet_addr = spawn_greet(&mut join_set, "0.0.0.0:0", req_msg, resp_msg, 1).await;
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
        let greet_addr = spawn_greet(&mut join_set, "[::]:0", req_msg, resp_msg, 1).await;

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
}
