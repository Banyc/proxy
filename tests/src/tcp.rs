#[cfg(test)]
mod tests {
    use std::{io, time::Duration};

    use common::{
        loading::Server,
        proxy_table::ProxyConfig,
        stream::{
            addr::StreamAddr,
            concrete::{
                addr::{ConcreteStreamAddr, ConcreteStreamType},
                context::StreamContext,
                created_stream::CreatedStreamAndAddr,
                pool::ConcreteConnPool,
            },
            proxy_table::StreamProxyConfig,
        },
    };
    use proxy_client::stream::{establish, trace_rtt};
    use proxy_server::stream::{
        kcp::build_kcp_proxy_server, mptcp::build_mptcp_proxy_server, tcp::build_tcp_proxy_server,
        StreamProxy,
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

    async fn spawn_proxy(
        join_set: &mut tokio::task::JoinSet<()>,
        addr: &str,
        ty: ConcreteStreamType,
    ) -> StreamProxyConfig<ConcreteStreamType> {
        let crypto = create_random_crypto();
        let proxy = StreamProxy::new(
            crypto.clone(),
            None,
            StreamContext {
                session_table: None,
                pool: Swap::new(ConcreteConnPool::empty()),
            },
        );
        let proxy_addr = match ty {
            ConcreteStreamType::Tcp => {
                let server = build_tcp_proxy_server(addr, proxy).await.unwrap();
                let proxy_addr = server.listener().local_addr().unwrap();
                join_set.spawn(async move {
                    let _handle = server.handle();
                    server.serve().await.unwrap();
                });
                proxy_addr
            }
            ConcreteStreamType::Kcp => {
                let server = build_kcp_proxy_server(addr, proxy).await.unwrap();
                let proxy_addr = server.listener().local_addr().unwrap();
                join_set.spawn(async move {
                    let _handle = server.handle();
                    server.serve().await.unwrap();
                });
                proxy_addr
            }
            ConcreteStreamType::Mptcp => {
                let server = build_mptcp_proxy_server(addr, proxy).await.unwrap();
                let proxy_addr = server.listener().local_addr().unwrap();
                join_set.spawn(async move {
                    let _handle = server.handle();
                    server.serve().await.unwrap();
                });
                proxy_addr
            }
        };
        ProxyConfig {
            address: StreamAddr {
                address: proxy_addr.into(),
                stream_type: ty,
            },
            crypto,
        }
    }

    async fn spawn_tcp_proxy(
        join_set: &mut tokio::task::JoinSet<()>,
        addr: &str,
    ) -> StreamProxyConfig<ConcreteStreamType> {
        spawn_proxy(join_set, addr, ConcreteStreamType::Tcp).await
    }

    async fn spawn_kcp_proxy(
        join_set: &mut tokio::task::JoinSet<()>,
        addr: &str,
    ) -> StreamProxyConfig<ConcreteStreamType> {
        spawn_proxy(join_set, addr, ConcreteStreamType::Kcp).await
    }

    async fn spawn_greet(
        join_set: &mut tokio::task::JoinSet<()>,
        addr: &str,
        req: &[u8],
        resp: &[u8],
        accepts: usize,
    ) -> ConcreteStreamAddr {
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
            stream_type: ConcreteStreamType::Tcp,
        }
    }

    async fn read_response<S>(stream: &mut S, resp_msg: &[u8]) -> io::Result<()>
    where
        S: AsyncRead + Unpin,
    {
        let mut buf = [0; 1024];
        let msg_buf = &mut buf[..resp_msg.len()];
        stream.read_exact(msg_buf).await.unwrap();
        assert_eq!(msg_buf, resp_msg);
        Ok(())
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_proxies() {
        let mut join_set = tokio::task::JoinSet::new();

        // Start proxy servers
        let proxy_1_config = spawn_tcp_proxy(&mut join_set, "0.0.0.0:0").await;
        let proxy_2_config = spawn_tcp_proxy(&mut join_set, "0.0.0.0:0").await;
        let proxy_3_config = spawn_tcp_proxy(&mut join_set, "0.0.0.0:0").await;
        let proxies = vec![proxy_1_config, proxy_2_config, proxy_3_config];

        // Message to send
        let req_msg = b"hello world";
        let resp_msg = b"goodbye world";

        // Start greet server
        let greet_addr = spawn_greet(&mut join_set, "[::]:0", req_msg, resp_msg, 1).await;

        // Connect to proxy server
        let CreatedStreamAndAddr { mut stream, .. } =
            establish(&proxies, greet_addr, &Swap::new(ConcreteConnPool::empty()))
                .await
                .unwrap();

        // Send message
        stream.write_all(req_msg).await.unwrap();

        // Read response
        read_response(&mut stream, resp_msg).await.unwrap();

        // Trace
        let rtt = trace_rtt(&proxies, &Swap::new(ConcreteConnPool::empty()))
            .await
            .unwrap();
        assert!(rtt > Duration::from_secs(0));
        assert!(rtt < Duration::from_secs(1));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_clients() {
        let mut join_set = tokio::task::JoinSet::new();

        // Start proxy servers
        let proxy_1_config = spawn_tcp_proxy(&mut join_set, "0.0.0.0:0").await;
        let proxy_2_config = spawn_tcp_proxy(&mut join_set, "0.0.0.0:0").await;
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
            handles.spawn(async move {
                // Connect to proxy server
                let CreatedStreamAndAddr { mut stream, .. } =
                    establish(&proxies, greet_addr, &Swap::new(ConcreteConnPool::empty()))
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

    async fn stress_test(ty: ConcreteStreamType) {
        let mut join_set = tokio::task::JoinSet::new();

        // Start proxy servers
        let mut proxies = Vec::new();
        for _ in 0..STRESS_CHAINS {
            let proxy_config = match ty {
                ConcreteStreamType::Tcp => spawn_tcp_proxy(&mut join_set, "0.0.0.0:0").await,
                ConcreteStreamType::Kcp => spawn_kcp_proxy(&mut join_set, "0.0.0.0:0").await,
                ConcreteStreamType::Mptcp => spawn_kcp_proxy(&mut join_set, "0.0.0.0:0").await,
            };
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
            handles.spawn(async move {
                for _ in 0..STRESS_SERIAL {
                    let greet_addr = greet_addr.clone();
                    // Connect to proxy server
                    let CreatedStreamAndAddr { mut stream, .. } =
                        establish(&proxies, greet_addr, &Swap::new(ConcreteConnPool::empty()))
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
        let CreatedStreamAndAddr { mut stream, .. } =
            establish(&[], greet_addr, &Swap::new(ConcreteConnPool::empty()))
                .await
                .unwrap();

        // Send message
        stream.write_all(req_msg).await.unwrap();

        // Read response
        read_response(&mut stream, resp_msg).await.unwrap();
    }
}
