#[cfg(test)]
mod tests {
    use std::net::SocketAddr;

    use client::udp_proxy_client::UdpProxySocket;
    use models::{ProxyProtocolError, ResponseErrorKind};
    use server::udp_proxy::UdpProxy;
    use tokio::net::UdpSocket;

    async fn spawn_proxy(addr: &str) -> SocketAddr {
        let listener = UdpSocket::bind(addr).await.unwrap();
        let proxy_addr = listener.local_addr().unwrap();
        let proxy = UdpProxy::new(listener);
        tokio::spawn(async move {
            proxy.serve().await.unwrap();
        });
        proxy_addr
    }

    async fn spawn_greet(addr: &str, req: &[u8], resp: &[u8], accepts: usize) -> SocketAddr {
        let listener = UdpSocket::bind(addr).await.unwrap();
        let greet_addr = listener.local_addr().unwrap();
        let req = req.to_vec();
        let resp = resp.to_vec();
        tokio::spawn(async move {
            for _ in 0..accepts {
                let mut buf = [0; 1024];
                let (len, addr) = listener.recv_from(&mut buf).await.unwrap();
                let msg_buf = &buf[..len];
                assert_eq!(msg_buf, req);
                listener.send_to(&resp, addr).await.unwrap();
            }
        });
        greet_addr
    }

    async fn read_response(
        socket: &UdpProxySocket,
        resp_msg: &[u8],
    ) -> Result<(), ProxyProtocolError> {
        let mut buf = [0; 1024];
        let n = socket.recv(&mut buf).await?;
        let msg_buf = &buf[..n];
        assert_eq!(msg_buf, resp_msg);
        Ok(())
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
        let greet_addr = spawn_greet("[::]:0", req_msg, resp_msg, 1).await;

        // Connect to proxy server
        let socket =
            UdpProxySocket::establish(vec![proxy_1_addr, proxy_2_addr, proxy_3_addr, greet_addr])
                .await
                .unwrap();

        // Send message
        socket.send(req_msg).await.unwrap();

        // Read response
        read_response(&socket, resp_msg).await.unwrap();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_clients() {
        // Start proxy servers
        let proxy_1_addr = spawn_proxy("0.0.0.0:0").await;
        let proxy_2_addr = spawn_proxy("0.0.0.0:0").await;

        // Message to send
        let req_msg = b"hello world";
        let resp_msg = b"goodbye world";

        let clients = 2;

        // Start greet server
        let greet_addr = spawn_greet("[::]:0", req_msg, resp_msg, clients).await;

        let mut handles = tokio::task::JoinSet::new();

        for _ in 0..clients {
            handles.spawn(async move {
                // Connect to proxy server
                let socket = UdpProxySocket::establish(vec![
                    proxy_1_addr,
                    proxy_2_addr,
                    // proxy_3_addr,
                    greet_addr,
                ])
                .await
                .unwrap();

                // Send message
                socket.send(req_msg).await.unwrap();

                // Read response
                read_response(&socket, resp_msg).await.unwrap();
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
        let greet_addr = spawn_greet("[::]:0", req_msg, resp_msg, usize::MAX).await;
        addresses.push(greet_addr);

        let mut handles = tokio::task::JoinSet::new();

        for _ in 0..100 {
            let addresses = addresses.clone();
            handles.spawn(async move {
                for _ in 0..10 {
                    // Connect to proxy server
                    let socket = UdpProxySocket::establish(addresses.clone()).await.unwrap();

                    // Send message
                    socket.send(req_msg).await.unwrap();

                    // Read response
                    read_response(&socket, resp_msg).await.unwrap();
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
        let greet_addr = spawn_greet("[::]:0", req_msg, resp_msg, 1).await;

        // Connect to proxy server
        let socket =
            UdpProxySocket::establish(vec![proxy_1_addr, proxy_2_addr, proxy_3_addr, greet_addr])
                .await
                .unwrap();

        // Send message
        socket.send(req_msg).await.unwrap();

        // Read response
        let err = read_response(&socket, resp_msg).await.unwrap_err();

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
        let greet_addr = spawn_greet("[::]:0", req_msg, resp_msg, 1).await;

        // Connect to proxy server
        let socket = UdpProxySocket::establish(vec![greet_addr]).await.unwrap();

        // Send message
        socket.send(req_msg).await.unwrap();

        // Read response
        read_response(&socket, resp_msg).await.unwrap();
    }
}