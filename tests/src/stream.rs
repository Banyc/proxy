#[cfg(test)]
mod tests {
    use std::{io, sync::Arc, time::Duration};

    use common::{
        anti_replay::{ReplayValidator, VALIDATOR_CAPACITY, VALIDATOR_TIME_FRAME},
        connect::ConnectorConfig,
        loading::Serve,
        proxy_table::ProxyConfig,
        stream::{
            addr::StreamAddr, conn::ConnAndAddr, pool::StreamConnPool,
            proxy_table::StreamProxyConfig,
        },
    };
    use protocol::stream::{
        addr::ConcreteStreamType, connect::ConcreteStreamConnectorTable,
        context::ConcreteStreamContext,
    };
    use proxy_client::stream::{establish, trace_rtt};
    use proxy_server::stream::{
        StreamProxyConnHandler, kcp::build_kcp_proxy_server, mptcp::build_mptcp_proxy_server,
        rtp::build_rtp_proxy_server, rtp_mux::build_rtp_mux_proxy_server,
        tcp::build_tcp_proxy_server, tcp_mux::build_tcp_mux_proxy_server,
    };
    use serial_test::serial;
    use swap::Swap;
    use tokio::{
        io::{AsyncRead, AsyncReadExt, AsyncWriteExt},
        net::TcpListener,
    };

    use crate::{STRESS_CHAINS, STRESS_PARALLEL, STRESS_SERIAL};

    fn create_random_crypto() -> tokio_chacha20::config::Config {
        let key: [u8; 32] = rand::random();
        tokio_chacha20::config::Config::new(key.into())
    }

    fn stream_context() -> ConcreteStreamContext {
        ConcreteStreamContext {
            session_table: None,
            pool: Swap::new(StreamConnPool::empty()),
            connector_table: Arc::new(
                ConcreteStreamConnectorTable::new(ConnectorConfig::default()),
            ),
            replay_validator: Arc::new(ReplayValidator::new(
                VALIDATOR_TIME_FRAME,
                VALIDATOR_CAPACITY,
            )),
        }
    }

    async fn spawn_proxy(
        join_set: &mut tokio::task::JoinSet<()>,
        addr: &Arc<str>,
        ty: ConcreteStreamType,
    ) -> StreamProxyConfig {
        let crypto = create_random_crypto();
        let proxy =
            StreamProxyConnHandler::new(crypto.clone(), None, stream_context(), Arc::clone(addr));
        let (set_conn_handler_tx, set_conn_handler_rx) = tokio::sync::mpsc::channel(64);
        let proxy_addr = match ty {
            ConcreteStreamType::Tcp => {
                let server = build_tcp_proxy_server(addr.as_ref(), proxy).await.unwrap();
                let proxy_addr = server.listener().local_addr().unwrap();
                join_set.spawn(async move {
                    let _set_conn_handler_tx = set_conn_handler_tx;
                    server.serve(set_conn_handler_rx).await.unwrap();
                });
                proxy_addr
            }
            ConcreteStreamType::TcpMux => {
                let server = build_tcp_mux_proxy_server(addr.as_ref(), proxy)
                    .await
                    .unwrap();
                let proxy_addr = server.listener().local_addr().unwrap();
                join_set.spawn(async move {
                    let _set_conn_handler_tx = set_conn_handler_tx;
                    server.serve(set_conn_handler_rx).await.unwrap();
                });
                proxy_addr
            }
            ConcreteStreamType::Kcp => {
                let server = build_kcp_proxy_server(addr.as_ref(), proxy).await.unwrap();
                let proxy_addr = server.listener().local_addr().unwrap();
                join_set.spawn(async move {
                    let _set_conn_handler_tx = set_conn_handler_tx;
                    server.serve(set_conn_handler_rx).await.unwrap();
                });
                proxy_addr
            }
            ConcreteStreamType::Mptcp => {
                let server = build_mptcp_proxy_server(addr.as_ref(), proxy)
                    .await
                    .unwrap();
                let proxy_addr = server.listener().local_addrs().next().unwrap().unwrap();
                join_set.spawn(async move {
                    let _set_conn_handler_tx = set_conn_handler_tx;
                    server.serve(set_conn_handler_rx).await.unwrap();
                });
                proxy_addr
            }
            ConcreteStreamType::Rtp => {
                let server = build_rtp_proxy_server(addr.as_ref(), proxy).await.unwrap();
                let proxy_addr = server.listener().local_addr();
                join_set.spawn(async move {
                    let _set_conn_handler_tx = set_conn_handler_tx;
                    server.serve(set_conn_handler_rx).await.unwrap();
                });
                proxy_addr
            }
            ConcreteStreamType::RtpMux => {
                let fec = false;
                let server = build_rtp_mux_proxy_server(addr.as_ref(), proxy, fec)
                    .await
                    .unwrap();
                let proxy_addr = server.listener().local_addr();
                join_set.spawn(async move {
                    let _set_conn_handler_tx = set_conn_handler_tx;
                    server.serve(set_conn_handler_rx).await.unwrap();
                });
                proxy_addr
            }
            ConcreteStreamType::RtpMuxFec => {
                let fec = true;
                let server = build_rtp_mux_proxy_server(addr.as_ref(), proxy, fec)
                    .await
                    .unwrap();
                let proxy_addr = server.listener().local_addr();
                join_set.spawn(async move {
                    let _set_conn_handler_tx = set_conn_handler_tx;
                    server.serve(set_conn_handler_rx).await.unwrap();
                });
                proxy_addr
            }
        };
        ProxyConfig {
            address: StreamAddr {
                address: proxy_addr.into(),
                stream_type: ty.to_string().into(),
            },
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
    ) -> StreamAddr {
        let listener = TcpListener::bind(addr).await.unwrap();
        let greet_addr = listener.local_addr().unwrap();
        let req = req.to_vec();
        let resp = resp.to_vec();
        join_set.spawn(async move {
            let mut join_set = tokio::task::JoinSet::new();
            for _ in 0..accepts {
                let (mut stream, _) = listener.accept().await.unwrap();
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
            while let Some(res) = join_set.join_next().await {
                res.unwrap();
            }
        });
        StreamAddr {
            address: greet_addr.into(),
            stream_type: ConcreteStreamType::Tcp.to_string().into(),
        }
    }

    async fn read_response<Stream>(stream: &mut Stream, resp_msg: &[u8]) -> io::Result<()>
    where
        Stream: AsyncRead + Unpin,
    {
        let mut buf = [0; 1024];
        let msg_buf = &mut buf[..resp_msg.len()];
        stream.read_exact(msg_buf).await.unwrap();
        assert_eq!(msg_buf, resp_msg);
        Ok(())
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_proxies() {
        let stream_context = stream_context();

        let mut join_set = tokio::task::JoinSet::new();

        // Start proxy servers
        let addr = Arc::from("0.0.0.0:0");
        let proxy_1_config = spawn_proxy(&mut join_set, &addr, ConcreteStreamType::Tcp).await;
        let proxy_2_config = spawn_proxy(&mut join_set, &addr, ConcreteStreamType::Tcp).await;
        let proxy_3_config = spawn_proxy(&mut join_set, &addr, ConcreteStreamType::Tcp).await;
        let proxies = vec![proxy_1_config, proxy_2_config, proxy_3_config];

        // Message to send
        let req_msg = b"hello world";
        let resp_msg = b"goodbye world";

        // Start greet server
        let greet_addr = spawn_greet(&mut join_set, "[::]:0", req_msg, resp_msg, 1).await;

        // Connect to proxy server
        let ConnAndAddr { mut stream, .. } = establish(&proxies, greet_addr, &stream_context)
            .await
            .unwrap();

        // Send message
        stream.write_all(req_msg).await.unwrap();

        // Read response
        read_response(&mut stream, resp_msg).await.unwrap();

        // Trace
        let rtt = trace_rtt(&proxies, &stream_context).await.unwrap();
        assert!(rtt > Duration::from_secs(0));
        assert!(rtt < Duration::from_secs(1));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_clients() {
        let stream_context = stream_context();

        let mut join_set = tokio::task::JoinSet::new();

        // Start proxy servers
        let addr = Arc::from("0.0.0.0:0");
        let proxy_1_config = spawn_proxy(&mut join_set, &addr, ConcreteStreamType::Tcp).await;
        let proxy_2_config = spawn_proxy(&mut join_set, &addr, ConcreteStreamType::Tcp).await;
        let proxies = vec![proxy_1_config, proxy_2_config];

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
            let stream_context = stream_context.clone();
            handles.spawn(async move {
                // Connect to proxy server
                let ConnAndAddr { mut stream, .. } =
                    establish(&proxies, greet_addr, &stream_context)
                        .await
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
    }

    #[tokio::test(flavor = "multi_thread")]
    #[serial]
    async fn stress_test_tcp() {
        stress_test(ConcreteStreamType::Tcp).await
    }

    #[tokio::test(flavor = "multi_thread")]
    #[serial]
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

    #[tokio::test(flavor = "multi_thread")]
    #[serial]
    async fn stress_test_rtp_mux_fec() {
        stress_test(ConcreteStreamType::RtpMuxFec).await
    }

    async fn stress_test(ty: ConcreteStreamType) {
        let stream_context = stream_context();

        let mut join_set = tokio::task::JoinSet::new();

        // Start proxy servers
        let mut proxies = Vec::new();
        let addr = Arc::from("0.0.0.0:0");
        for _ in 0..STRESS_CHAINS {
            let proxy_config = spawn_proxy(&mut join_set, &addr, ty).await;
            proxies.push(proxy_config);
        }

        // Message to send
        let req_msg = b"hello world";
        let resp_msg = b"goodbye world";

        // Start greet server
        let greet_addr = spawn_greet(&mut join_set, "[::]:0", req_msg, resp_msg, usize::MAX).await;

        let mut handles = tokio::task::JoinSet::new();

        for _ in 0..STRESS_PARALLEL {
            let proxies = proxies.clone();
            let greet_addr = greet_addr.clone();
            let stream_context = stream_context.clone();
            handles.spawn(async move {
                for _ in 0..STRESS_SERIAL {
                    let greet_addr = greet_addr.clone();
                    // Connect to proxy server
                    let ConnAndAddr { mut stream, .. } =
                        establish(&proxies, greet_addr, &stream_context)
                            .await
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
    }

    // async fn test_bad_proxy() {
    //     // Start proxy servers
    //     let proxy_1_config = spawn_proxy("localhost:0").await;
    //     let proxy_2_config = spawn_proxy("localhost:0").await;
    //     let proxy_3_config = spawn_proxy("localhost:0").await;

    //     // Message to send
    //     let req_msg = b"hello world";
    //     let resp_msg = b"goodbye world";

    //     // Start greet server
    //     let greet_addr = spawn_greet("[::]:0", req_msg, resp_msg, 1).await;

    //     // Connect to proxy server
    //     let err = establish(
    //         &[proxy_1_config.clone(), proxy_2_config, proxy_3_config],
    //         greet_addr,
    //         &ConcreteConnPool::empty(),
    //     )
    //     .await
    //     .unwrap_err();
    //     match err {
    //         ProxyProtocolError::Response(err) => {
    //             match err.kind {
    //                 ResponseErrorKind::Loopback => {}
    //                 _ => panic!("Unexpected error: {:?}", err),
    //             }
    //             assert_eq!(err.source, proxy_1_config.address.address);
    //         }
    //         _ => panic!("Unexpected error: {:?}", err),
    //     }
    // }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_no_proxies() {
        // Start proxy servers

        let mut join_set = tokio::task::JoinSet::new();

        // Message to send
        let req_msg = b"hello world";
        let resp_msg = b"goodbye world";

        // Start greet server
        let greet_addr = spawn_greet(&mut join_set, "[::]:0", req_msg, resp_msg, 1).await;

        // Connect to proxy server
        let ConnAndAddr { mut stream, .. } =
            establish(&[], greet_addr, &stream_context()).await.unwrap();

        // Send message
        stream.write_all(req_msg).await.unwrap();

        // Read response
        read_response(&mut stream, resp_msg).await.unwrap();
    }
}
