#[cfg(test)]
mod tests {
    use std::{
        sync::{Arc, RwLock},
        time::Duration,
    };

    use bytes::BytesMut;
    use common::{
        addr::InternetAddr,
        anti_replay::{TimeValidator, VALIDATOR_TIME_FRAME, VALIDATOR_UDP_HDR_TTL},
        connect::ConnectorConfig,
        header::route::RouteErrorKind,
        loading::Serve,
        proto::{
            client::{
                self,
                udp::{UdpProxyClient, UdpProxyClientReadHalf, trace_rtt},
            },
            connect::udp::UdpConnector,
            context::UdpContext,
            proxy_table::UdpProxyConfig,
        },
        proxy_table::ProxyConfig,
        udp::PACKET_BUFFER_LENGTH,
    };
    use proxy_server::udp::UdpProxyConnHandler;
    use serial_test::serial;
    use tokio::net::UdpSocket;

    use crate::{STRESS_CHAINS, STRESS_PARALLEL, STRESS_SERIAL};

    fn create_random_crypto() -> tokio_chacha20::config::Config {
        let key: [u8; 32] = rand::random();
        tokio_chacha20::config::Config::new(key.into())
    }

    fn udp_context() -> UdpContext {
        UdpContext {
            session_table: None,
            connector: Arc::new(UdpConnector::new(Arc::new(RwLock::new(
                ConnectorConfig::default(),
            )))),
            time_validator: Arc::new(TimeValidator::new(
                VALIDATOR_TIME_FRAME + VALIDATOR_UDP_HDR_TTL,
            )),
        }
    }

    async fn spawn_proxy(join_set: &mut tokio::task::JoinSet<()>, addr: &str) -> UdpProxyConfig {
        let crypto = create_random_crypto();
        let proxy = UdpProxyConnHandler::new(crypto.clone(), None, udp_context());
        let server = proxy.build(addr).await.unwrap();
        let proxy_addr = server.listener().local_addr().unwrap();
        join_set.spawn(async move {
            let (_set_conn_handler_tx, set_conn_handler_rx) = tokio::sync::mpsc::channel(64);
            server.serve(set_conn_handler_rx).await.unwrap();
        });
        ProxyConfig {
            address: proxy_addr.into(),
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
        let n = client.recv(&mut buf).await?;
        let msg_buf = &buf[..n];
        assert_eq!(msg_buf, resp_msg);
        Ok(())
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_proxies() {
        let context = udp_context();

        let mut pkt_buf = BytesMut::with_capacity(PACKET_BUFFER_LENGTH);
        let mut join_set = tokio::task::JoinSet::new();

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
        let client = UdpProxyClient::establish(proxies.clone(), greet_addr, &context)
            .await
            .unwrap();
        let (mut client_read, mut client_write) = client.into_split();

        // Send message
        client_write.send(req_msg).await.unwrap();

        // Read response
        read_response(&mut client_read, resp_msg).await.unwrap();

        // Trace
        let rtt = trace_rtt(&mut pkt_buf, &proxies, &context).await.unwrap();
        assert!(rtt > Duration::from_secs(0));
        assert!(rtt < Duration::from_secs(1));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_clients() {
        let context = udp_context();

        let mut join_set = tokio::task::JoinSet::new();

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
                let client = UdpProxyClient::establish(proxies, greet_addr, &context)
                    .await
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
        let context = udp_context();

        let mut join_set = tokio::task::JoinSet::new();

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
                    let client = UdpProxyClient::establish(proxies.clone(), greet_addr, &context)
                        .await
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
        let context = udp_context();

        let mut join_set = tokio::task::JoinSet::new();

        // Start proxy servers
        let proxy_1_config = spawn_proxy(&mut join_set, "localhost:0").await;
        let proxy_2_config = spawn_proxy(&mut join_set, "localhost:0").await;
        let proxy_3_config = spawn_proxy(&mut join_set, "localhost:0").await;

        // Message to send
        let req_msg = b"hello world";
        let resp_msg = b"goodbye world";

        // Start greet server
        let greet_addr = spawn_greet(&mut join_set, "[::]:0", req_msg, resp_msg, 1).await;

        // Connect to proxy server
        let client = UdpProxyClient::establish(
            vec![proxy_1_config.clone(), proxy_2_config, proxy_3_config].into(),
            greet_addr,
            &context,
        )
        .await
        .unwrap();
        let (mut client_read, mut client_write) = client.into_split();

        // Send message
        client_write.send(req_msg).await.unwrap();

        // Read response
        let err = read_response(&mut client_read, resp_msg).await.unwrap_err();

        match err {
            client::udp::RecvError::Response { err, addr } => {
                match err.kind {
                    RouteErrorKind::Loopback => {}
                    _ => panic!("Unexpected error: {err:?}"),
                }
                assert_eq!(addr, proxy_1_config.address);
            }
            _ => panic!("Unexpected error: {err:?}"),
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_no_proxies() {
        let context = udp_context();

        // Start proxy servers

        let mut join_set = tokio::task::JoinSet::new();

        // Message to send
        let req_msg = b"hello world";
        let resp_msg = b"goodbye world";

        // Start greet server
        let greet_addr = spawn_greet(&mut join_set, "[::]:0", req_msg, resp_msg, 1).await;

        // Connect to proxy server
        let client = UdpProxyClient::establish(vec![].into(), greet_addr, &context)
            .await
            .unwrap();
        let (mut client_read, mut client_write) = client.into_split();

        // Send message
        client_write.send(req_msg).await.unwrap();

        // Read response
        read_response(&mut client_read, resp_msg).await.unwrap();
    }
}
