#[cfg(test)]
mod tests {
    use std::{sync::Arc, time::Duration};

    use common::{
        addr::InternetAddr, header::route::RouteErrorKind, loading::Server,
        proxy_table::ProxyConfig, udp::proxy_table::UdpProxyConfig,
    };
    use proxy_client::udp::{trace_rtt, RecvError, UdpProxyClient, UdpProxyClientReadHalf};
    use proxy_server::udp::UdpProxy;
    use tokio::net::UdpSocket;

    use crate::create_random_crypto;

    async fn spawn_proxy(addr: &str) -> UdpProxyConfig {
        let crypto = create_random_crypto();
        let proxy = UdpProxy::new(crypto.clone(), None);
        let server = proxy.build(addr).await.unwrap();
        let proxy_addr = server.listener().local_addr().unwrap();
        tokio::spawn(async move {
            let _handle = server.handle().clone();
            server.serve().await.unwrap();
        });
        ProxyConfig {
            address: proxy_addr.into(),
            crypto,
        }
    }

    async fn spawn_greet(addr: &str, req: &[u8], resp: &[u8], accepts: usize) -> InternetAddr {
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
        greet_addr.into()
    }

    async fn read_response(
        client: &mut UdpProxyClientReadHalf,
        resp_msg: &[u8],
    ) -> Result<(), RecvError> {
        let mut buf = [0; 1024];
        let n = client.recv(&mut buf).await?;
        let msg_buf = &buf[..n];
        assert_eq!(msg_buf, resp_msg);
        Ok(())
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_proxies() {
        // Start proxy servers
        let proxy_1_config = spawn_proxy("0.0.0.0:0").await;
        let proxy_2_config = spawn_proxy("0.0.0.0:0").await;
        let proxy_3_config = spawn_proxy("0.0.0.0:0").await;
        let proxies: Arc<[_]> = vec![proxy_1_config, proxy_2_config, proxy_3_config].into();

        // Message to send
        let req_msg = b"hello world";
        let resp_msg = b"goodbye world";

        // Start greet server
        let greet_addr = spawn_greet("[::]:0", req_msg, resp_msg, 1).await;

        // Connect to proxy server
        let client = UdpProxyClient::establish(proxies.clone(), greet_addr)
            .await
            .unwrap();
        let (mut client_read, mut client_write) = client.into_split();

        // Send message
        client_write.send(req_msg).await.unwrap();

        // Read response
        read_response(&mut client_read, resp_msg).await.unwrap();

        // Trace
        let rtt = trace_rtt(&proxies).await.unwrap();
        assert!(rtt > Duration::from_secs(0));
        assert!(rtt < Duration::from_secs(1));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_clients() {
        // Start proxy servers
        let proxy_1_config = spawn_proxy("0.0.0.0:0").await;
        let proxy_2_config = spawn_proxy("0.0.0.0:0").await;
        let proxies: Arc<[_]> = vec![proxy_1_config, proxy_2_config].into();

        // Message to send
        let req_msg = b"hello world";
        let resp_msg = b"goodbye world";

        let clients = 2;

        // Start greet server
        let greet_addr = spawn_greet("[::]:0", req_msg, resp_msg, clients).await;

        let mut handles = tokio::task::JoinSet::new();

        for _ in 0..clients {
            let proxies = proxies.clone();
            let greet_addr = greet_addr.clone();
            handles.spawn(async move {
                // Connect to proxy server
                let client = UdpProxyClient::establish(proxies, greet_addr)
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
    async fn stress_test() {
        // Start proxy servers
        let mut proxies = Vec::new();
        for _ in 0..10 {
            let proxy_config = spawn_proxy("0.0.0.0:0").await;
            proxies.push(proxy_config);
        }
        let proxies: Arc<[_]> = proxies.into();

        // Message to send
        let req_msg = b"hello world";
        let resp_msg = b"goodbye world";

        // Start greet server
        let greet_addr = spawn_greet("[::]:0", req_msg, resp_msg, usize::MAX).await;

        let mut handles = tokio::task::JoinSet::new();

        for _ in 0..100 {
            let proxies = proxies.clone();
            let greet_addr = greet_addr.clone();
            handles.spawn(async move {
                for _ in 0..10 {
                    let greet_addr = greet_addr.clone();
                    // Connect to proxy server
                    let client = UdpProxyClient::establish(proxies.clone(), greet_addr)
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
        let client = UdpProxyClient::establish(
            vec![proxy_1_config.clone(), proxy_2_config, proxy_3_config].into(),
            greet_addr,
        )
        .await
        .unwrap();
        let (mut client_read, mut client_write) = client.into_split();

        // Send message
        client_write.send(req_msg).await.unwrap();

        // Read response
        let err = read_response(&mut client_read, resp_msg).await.unwrap_err();

        match err {
            RecvError::Response { err, addr } => {
                match err.kind {
                    RouteErrorKind::Loopback => {}
                    _ => panic!("Unexpected error: {:?}", err),
                }
                assert_eq!(addr, proxy_1_config.address);
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
        let client = UdpProxyClient::establish(vec![].into(), greet_addr)
            .await
            .unwrap();
        let (mut client_read, mut client_write) = client.into_split();

        // Send message
        client_write.send(req_msg).await.unwrap();

        // Read response
        read_response(&mut client_read, resp_msg).await.unwrap();
    }
}
