#[cfg(test)]
mod tests {
    use std::{io, sync::Arc, time::Duration};

    use ae::anti_replay::ReplayValidator;
    use common::{
        anti_replay::{VALIDATOR_CAPACITY, VALIDATOR_TIME_FRAME},
        connect::{ConnectorConfig, ConnectorReset},
        loading::{self, Serve},
        notify::Notify,
        proto::{
            addr::StreamAddr,
            client::stream::{establish, trace_rtt},
            conn::stream::ConnAndAddr,
            conn_handler::stream::StreamProxyConnHandler,
            context::StreamContext,
            route::StreamConnConfig,
        },
        route::ConnConfig,
        stream::pool::StreamConnPool,
    };
    use protocol::stream::{
        addr::ConcreteStreamType,
        connect::build_concrete_stream_connector_table,
        streams::{
            kcp::build_kcp_proxy_server, mptcp::build_mptcp_proxy_server,
            rtp::build_rtp_proxy_server, rtp_mux::build_rtp_mux_proxy_server,
            tcp::proxy_server::build_tcp_proxy_server, tcp_mux::build_tcp_mux_proxy_server,
        },
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

    fn stream_context() -> StreamContext {
        let connector_reset = ConnectorReset(Notify::new());
        StreamContext {
            session_table: None,
            pool: Swap::new(StreamConnPool::empty()),
            connector_table: Arc::new(build_concrete_stream_connector_table(
                ConnectorConfig::default(),
                connector_reset,
            )),
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
    ) -> StreamConnConfig {
        let crypto = create_random_crypto();
        let proxy =
            StreamProxyConnHandler::new(crypto.clone(), None, stream_context(), Arc::clone(addr));
        let (set_conn_handler_tx, set_conn_handler_rx) = loading::replace_conn_handler_channel();
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
        ConnConfig {
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
    #[ignore]
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
        tokio::time::sleep(Duration::from_secs_f64(0.6)).await;

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
    #[serial]
    #[ignore]
    async fn perf_bulk_rtp_mux_fec() {
        use std::time::Instant;

        tokio::time::sleep(Duration::from_secs_f64(0.6)).await;

        let stream_context = stream_context();

        let mut join_set = tokio::task::JoinSet::new();

        // Start proxy servers
        let mut proxies = Vec::new();
        let addr = Arc::from("0.0.0.0:0");
        for _ in 0..STRESS_CHAINS {
            let proxy_config =
                spawn_proxy(&mut join_set, &addr, ConcreteStreamType::RtpMuxFec).await;
            proxies.push(proxy_config);
        }

        // Local TCP receiver: reads exactly TOTAL_BYTES then sends 1-byte ack
        const TOTAL_BYTES: usize = 32 * 1024 * 1024;
        const CHUNK: usize = 64 * 1024;

        let listener = TcpListener::bind("[::]:0").await.unwrap();
        let receiver_addr = listener.local_addr().unwrap();
        let receiver_greet_addr = StreamAddr {
            address: receiver_addr.into(),
            stream_type: ConcreteStreamType::Tcp.to_string().into(),
        };

        join_set.spawn(async move {
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

        // Establish a single stream through the proxy chain
        let ConnAndAddr { mut stream, .. } =
            establish(&proxies, receiver_greet_addr, &stream_context)
                .await
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
        println!("perf_bulk_rtp_mux_fec_mib_s={mib_s:.3}");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_no_proxies() {
        let mut join_set = tokio::task::JoinSet::new();

        let req_msg = b"hello world";
        let resp_msg = b"goodbye world";

        let greet_addr = spawn_greet(&mut join_set, "[::]:0", req_msg, resp_msg, 1).await;

        let ConnAndAddr { mut stream, .. } =
            establish(&[], greet_addr, &stream_context()).await.unwrap();

        stream.write_all(req_msg).await.unwrap();
        read_response(&mut stream, resp_msg).await.unwrap();
    }

    #[tokio::test(flavor = "multi_thread")]
    #[serial]
    async fn test_rtp_mux_migration_integrity() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        tokio::time::sleep(Duration::from_secs_f64(0.6)).await;

        let stream_context = stream_context();
        let mut join_set = tokio::task::JoinSet::new();

        // Start proxy server
        let addr = Arc::from("0.0.0.0:0");
        let proxy_config =
            spawn_proxy(&mut join_set, &addr, ConcreteStreamType::RtpMux).await;
        let proxies = vec![proxy_config];

        // Start an echo server that reads a 4-byte length prefix then echoes
        // back exactly that many bytes. This lets us test both directions
        // independently.
        let listener = TcpListener::bind("[::]:0").await.unwrap();
        let echo_addr = listener.local_addr().unwrap();
        let greet_addr = StreamAddr {
            address: echo_addr.into(),
            stream_type: ConcreteStreamType::Tcp.to_string().into(),
        };
        join_set.spawn(async move {
            loop {
                let (mut sock, _) = match listener.accept().await {
                    Ok(x) => x,
                    Err(_) => break,
                };
                tokio::spawn(async move {
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
        });

        let concurrent = 4;
        let mut handles = tokio::task::JoinSet::new();

        for stream_idx in 0..concurrent {
            let proxies = proxies.clone();
            let greet_addr = greet_addr.clone();
            let stream_context = stream_context.clone();
            handles.spawn(async move {
                let ConnAndAddr { mut stream, .. } =
                    establish(&proxies, greet_addr, &stream_context)
                        .await
                        .unwrap();

                // Large burst >2048 → bulk lane
                let large: Vec<u8> = (0..4096u16).map(|i| ((i + stream_idx as u16) % 256) as u8).collect();
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
                    assert_eq!(small_echo, small, "small echo mismatch stream {stream_idx} iter {i}");
                }
            });
        }

        while let Some(x) = handles.join_next().await {
            x.unwrap();
        }
    }
}
