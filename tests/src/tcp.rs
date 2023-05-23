#[cfg(test)]
mod tests {
    use std::{io, net::SocketAddr};

    use common::{
        error::{ProxyProtocolError, ResponseErrorKind},
        header::ProxyConfig,
        quic::QuicPersistentConnections,
    };
    use proxy_client::tcp_proxy_client::TcpProxyStream;
    use proxy_server::stream_proxy_server::StreamProxyServer;
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::TcpListener,
    };

    use crate::create_random_crypto;

    async fn spawn_proxy(addr: &str) -> ProxyConfig {
        let crypto = create_random_crypto();
        let proxy = StreamProxyServer::new(
            crypto.clone(),
            None,
            QuicPersistentConnections::new(Default::default()),
        );
        let server = proxy.build(addr).await.unwrap();
        let proxy_addr = server.listener().local_addr().unwrap();
        tokio::spawn(async move {
            server.serve().await.unwrap();
        });
        ProxyConfig {
            address: proxy_addr.into(),
            crypto,
        }
    }

    async fn spawn_greet(addr: &str, req: &[u8], resp: &[u8], accepts: usize) -> SocketAddr {
        let listener = TcpListener::bind(addr).await.unwrap();
        let greet_addr = listener.local_addr().unwrap();
        let req = req.to_vec();
        let resp = resp.to_vec();
        tokio::spawn(async move {
            for _ in 0..accepts {
                let (mut stream, _) = listener.accept().await.unwrap();
                let req = req.to_vec();
                let resp = resp.to_vec();
                tokio::spawn(async move {
                    let mut buf = [0; 1024];
                    let msg_buf = &mut buf[..req.len()];
                    stream.read_exact(msg_buf).await.unwrap();
                    assert_eq!(msg_buf, req);
                    stream.write_all(&resp).await.unwrap();
                });
            }
        });
        greet_addr
    }

    async fn read_response(stream: &mut TcpProxyStream, resp_msg: &[u8]) -> io::Result<()> {
        let mut buf = [0; 1024];
        let msg_buf = &mut buf[..resp_msg.len()];
        stream.read_exact(msg_buf).await.unwrap();
        assert_eq!(msg_buf, resp_msg);
        Ok(())
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_proxies() {
        // Start proxy servers
        let proxy_1_config = spawn_proxy("0.0.0.0:0").await;
        let proxy_2_config = spawn_proxy("0.0.0.0:0").await;
        let proxy_3_config = spawn_proxy("0.0.0.0:0").await;

        // Message to send
        let req_msg = b"hello world";
        let resp_msg = b"goodbye world";

        // Start greet server
        let greet_addr = spawn_greet("[::]:0", req_msg, resp_msg, 1).await;

        // Connect to proxy server
        let mut stream = TcpProxyStream::establish(
            &[proxy_1_config, proxy_2_config, proxy_3_config],
            &greet_addr.into(),
        )
        .await
        .unwrap();

        // Send message
        stream.write_all(req_msg).await.unwrap();

        // Read response
        read_response(&mut stream, resp_msg).await.unwrap();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_clients() {
        // Start proxy servers
        let proxy_1_config = spawn_proxy("0.0.0.0:0").await;
        let proxy_2_config = spawn_proxy("0.0.0.0:0").await;
        let proxy_configs = vec![proxy_1_config, proxy_2_config];

        // Message to send
        let req_msg = b"hello world";
        let resp_msg = b"goodbye world";

        let clients = 2;

        // Start greet server
        let greet_addr = spawn_greet("[::]:0", req_msg, resp_msg, clients).await;

        let mut handles = tokio::task::JoinSet::new();

        for _ in 0..clients {
            let proxy_configs = proxy_configs.clone();
            handles.spawn(async move {
                // Connect to proxy server
                let mut stream = TcpProxyStream::establish(&proxy_configs, &greet_addr.into())
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
    async fn stress_test() {
        // Start proxy servers
        let mut proxy_configs = Vec::new();
        for _ in 0..10 {
            let proxy_config = spawn_proxy("0.0.0.0:0").await;
            proxy_configs.push(proxy_config);
        }

        // Message to send
        let req_msg = b"hello world";
        let resp_msg = b"goodbye world";

        // Start greet server
        let greet_addr = spawn_greet("[::]:0", req_msg, resp_msg, usize::MAX).await;

        let mut handles = tokio::task::JoinSet::new();

        for _ in 0..50 {
            let proxy_configs = proxy_configs.clone();
            handles.spawn(async move {
                for _ in 0..10 {
                    // Connect to proxy server
                    let mut stream = TcpProxyStream::establish(&proxy_configs, &greet_addr.into())
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

    #[tokio::test(flavor = "multi_thread")]
    async fn test_bad_proxy() {
        // Start proxy servers
        let proxy_1_config = spawn_proxy("localhost:0").await;
        let proxy_2_config = spawn_proxy("localhost:0").await;
        let proxy_3_config = spawn_proxy("localhost:0").await;

        // Message to send
        let req_msg = b"hello world";
        let resp_msg = b"goodbye world";

        // Start greet server
        let greet_addr = spawn_greet("[::]:0", req_msg, resp_msg, 1).await;

        // Connect to proxy server
        let err = TcpProxyStream::establish(
            &[proxy_1_config.clone(), proxy_2_config, proxy_3_config],
            &greet_addr.into(),
        )
        .await
        .unwrap_err();
        match err {
            ProxyProtocolError::Response(err) => {
                match err.kind {
                    ResponseErrorKind::Loopback => {}
                    _ => panic!("Unexpected error: {:?}", err),
                }
                assert_eq!(err.source, proxy_1_config.address);
            }
            _ => panic!("Unexpected error: {:?}", err),
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_no_proxies() {
        // Start proxy servers

        // Message to send
        let req_msg = b"hello world";
        let resp_msg = b"goodbye world";

        // Start greet server
        let greet_addr = spawn_greet("[::]:0", req_msg, resp_msg, 1).await;

        // Connect to proxy server
        let mut stream = TcpProxyStream::establish(&[], &greet_addr.into())
            .await
            .unwrap();

        // Send message
        stream.write_all(req_msg).await.unwrap();

        // Read response
        read_response(&mut stream, resp_msg).await.unwrap();
    }
}
