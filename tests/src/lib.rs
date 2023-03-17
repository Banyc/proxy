#[cfg(test)]
mod tests {
    use std::net::SocketAddr;

    use client::tcp_proxy_client::TcpProxyStream;
    use models::{ProxyProtocolError, ResponseErrorKind};
    use server::tcp_proxy::TcpProxy;
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::TcpListener,
    };

    async fn spawn_proxy(addr: &str) -> SocketAddr {
        let listener = TcpListener::bind(addr).await.unwrap();
        let proxy_addr = listener.local_addr().unwrap();
        let proxy = TcpProxy::new(listener);
        tokio::spawn(async move {
            proxy.serve().await.unwrap();
        });
        proxy_addr
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_proxies() {
        // Start proxy servers
        let proxy_1_addr = spawn_proxy("0.0.0.0:0").await;
        let proxy_2_addr = spawn_proxy("0.0.0.0:0").await;
        let proxy_3_addr = spawn_proxy("0.0.0.0:0").await;

        // Message to send
        let req_msg = b"hello world";
        let resp_msg = b"goodbye world";

        // Start greet server
        let greet_addr = {
            let listener = TcpListener::bind("[::]:0").await.unwrap();
            let greet_addr = listener.local_addr().unwrap();
            tokio::spawn(async move {
                let (mut stream, _) = listener.accept().await.unwrap();
                let mut buf = [0; 1024];
                let mut msg_buf = &mut buf[..req_msg.len()];
                stream.read_exact(&mut msg_buf).await.unwrap();
                assert_eq!(msg_buf, req_msg);
                stream.write_all(resp_msg).await.unwrap();
            });
            greet_addr
        };

        // Connect to proxy server
        let mut stream =
            TcpProxyStream::establish(&vec![proxy_1_addr, proxy_2_addr, proxy_3_addr, greet_addr])
                .await
                .unwrap();

        // Send message
        stream.write_all(req_msg).await.unwrap();

        // Read response
        let mut buf = [0; 1024];
        let mut msg_buf = &mut buf[..resp_msg.len()];
        stream.read_exact(&mut msg_buf).await.unwrap();
        assert_eq!(msg_buf, resp_msg);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_clients() {
        // Start proxy servers
        let proxy_1_addr = spawn_proxy("0.0.0.0:0").await;
        let proxy_2_addr = spawn_proxy("0.0.0.0:0").await;
        // let proxy_3_addr = spawn_proxy("0.0.0.0:0").await;

        // Message to send
        let req_msg = b"hello world";
        let resp_msg = b"goodbye world";

        // Start greet server
        let greet_addr = {
            let listener = TcpListener::bind("[::]:0").await.unwrap();
            let greet_addr = listener.local_addr().unwrap();
            tokio::spawn(async move {
                loop {
                    let (mut stream, _) = listener.accept().await.unwrap();
                    tokio::spawn(async move {
                        let mut buf = [0; 1024];
                        let mut msg_buf = &mut buf[..req_msg.len()];
                        stream.read_exact(&mut msg_buf).await.unwrap();
                        assert_eq!(msg_buf, req_msg);
                        stream.write_all(resp_msg).await.unwrap();
                    });
                }
            });
            greet_addr
        };

        let mut handles = tokio::task::JoinSet::new();

        for _ in 0..2 {
            handles.spawn(async move {
                // Connect to proxy server
                let mut stream = TcpProxyStream::establish(&vec![
                    proxy_1_addr,
                    proxy_2_addr,
                    // proxy_3_addr,
                    greet_addr,
                ])
                .await
                .unwrap();

                // Send message
                stream.write_all(req_msg).await.unwrap();

                // Read response
                let mut buf = [0; 1024];
                let mut msg_buf = &mut buf[..resp_msg.len()];
                stream.read_exact(&mut msg_buf).await.unwrap();
                assert_eq!(msg_buf, resp_msg);

                stream.close_gracefully().await.unwrap();
            });
        }

        while let Some(x) = handles.join_next().await {
            x.unwrap();
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn stress_test() {
        // Start proxy servers
        let mut addresses = Vec::new();
        for _ in 0..10 {
            let proxy_addr = spawn_proxy("0.0.0.0:0").await;
            addresses.push(proxy_addr);
        }

        // Message to send
        let req_msg = b"hello world";
        let resp_msg = b"goodbye world";

        // Start greet server
        let greet_addr = {
            let listener = TcpListener::bind("[::]:0").await.unwrap();
            let greet_addr = listener.local_addr().unwrap();
            tokio::spawn(async move {
                loop {
                    let (mut stream, _) = listener.accept().await.unwrap();
                    tokio::spawn(async move {
                        let mut buf = [0; 1024];
                        let mut msg_buf = &mut buf[..req_msg.len()];
                        stream.read_exact(&mut msg_buf).await.unwrap();
                        assert_eq!(msg_buf, req_msg);
                        stream.write_all(resp_msg).await.unwrap();
                    });
                }
            });
            greet_addr
        };
        addresses.push(greet_addr);

        let mut handles = tokio::task::JoinSet::new();

        for _ in 0..100 {
            let addresses = addresses.clone();
            handles.spawn(async move {
                for _ in 0..10 {
                    // Connect to proxy server
                    let mut stream = TcpProxyStream::establish(&addresses).await.unwrap();

                    // Send message
                    stream.write_all(req_msg).await.unwrap();

                    // Read response
                    let mut buf = [0; 1024];
                    let mut msg_buf = &mut buf[..resp_msg.len()];
                    stream.read_exact(&mut msg_buf).await.unwrap();
                    assert_eq!(msg_buf, resp_msg);

                    stream.close_gracefully().await.unwrap();
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
        let proxy_1_addr = spawn_proxy("localhost:0").await;
        let proxy_2_addr = spawn_proxy("localhost:0").await;
        let proxy_3_addr = spawn_proxy("localhost:0").await;

        // Message to send
        let req_msg = b"hello world";
        let resp_msg = b"goodbye world";

        // Start greet server
        let greet_addr = {
            let listener = TcpListener::bind("[::]:0").await.unwrap();
            let greet_addr = listener.local_addr().unwrap();
            tokio::spawn(async move {
                let (mut stream, _) = listener.accept().await.unwrap();
                let mut buf = [0; 1024];
                let mut msg_buf = &mut buf[..req_msg.len()];
                stream.read_exact(&mut msg_buf).await.unwrap();
                assert_eq!(msg_buf, req_msg);
                stream.write_all(resp_msg).await.unwrap();
            });
            greet_addr
        };

        // Connect to proxy server
        let err =
            TcpProxyStream::establish(&vec![proxy_1_addr, proxy_2_addr, proxy_3_addr, greet_addr])
                .await
                .unwrap_err();
        match err {
            ProxyProtocolError::Response(err) => {
                match err.kind {
                    ResponseErrorKind::Loopback => {}
                    _ => panic!("Unexpected error: {:?}", err),
                }
                assert_eq!(err.source, proxy_1_addr);
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
        let greet_addr = {
            let listener = TcpListener::bind("[::]:0").await.unwrap();
            let greet_addr = listener.local_addr().unwrap();
            tokio::spawn(async move {
                let (mut stream, _) = listener.accept().await.unwrap();
                let mut buf = [0; 1024];
                let mut msg_buf = &mut buf[..req_msg.len()];
                stream.read_exact(&mut msg_buf).await.unwrap();
                assert_eq!(msg_buf, req_msg);
                stream.write_all(resp_msg).await.unwrap();
            });
            greet_addr
        };

        // Connect to proxy server
        let mut stream = TcpProxyStream::establish(&vec![greet_addr]).await.unwrap();

        // Send message
        stream.write_all(req_msg).await.unwrap();

        // Read response
        let mut buf = [0; 1024];
        let mut msg_buf = &mut buf[..resp_msg.len()];
        stream.read_exact(&mut msg_buf).await.unwrap();
        assert_eq!(msg_buf, resp_msg);
    }
}
