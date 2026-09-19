//! End-to-end exercise of the UDP access server's datagram path over real
//! UDP sockets: the production handler is bound through its real `UdpServer`
//! and driven by a wire-level UDP client, asserting byte-exact forwarding to
//! the configured destination and back.
//!
//! Listeners bind `127.0.0.1:0`; the test reads the actual port, so it never
//! races a fixed port. Every background task lives in the test's `JoinSet`,
//! which aborts the lot when the test returns.

use std::{sync::Arc, time::Duration};

use ae::anti_replay::TimeValidator;
use common::{
    addr::InternetAddr,
    anti_replay::{VALIDATOR_TIME_FRAME, VALIDATOR_UDP_HDR_TTL},
    connect::{ConnectorConfig, connector_config_cell},
    lifecycle::retention::{RetentionActor, RetentionActorSender},
    loading::{self, Serve},
    proxy_runtime::{connect::udp::UdpConnector, context::UdpRuntime},
    route::RouteSelector,
    session::SessionSpawner,
};
use protocol::udp_proto::access_server::UdpAccessConnHandler;
use tokio::{net::UdpSocket, task::JoinSet};

type Tasks = JoinSet<()>;

fn spawn_process_actors(tasks: &mut Tasks) -> (SessionSpawner, RetentionActorSender) {
    let (session_spawner, mut session_rx) = SessionSpawner::channel();
    tasks.spawn(async move {
        let mut sessions = tokio::task::JoinSet::new();
        loop {
            tokio::select! {
                Some(fut) = session_rx.recv() => { sessions.spawn(fut); }
                Some(res) = sessions.join_next() => {
                    let _ = res.expect("session task panicked");
                }
                else => break,
            }
        }
    });
    let (retention_actor, retention) = RetentionActor::new();
    tasks.spawn(async move {
        let _exit = retention_actor.run().await;
    });
    (session_spawner, retention)
}

fn udp_context(tasks: &mut Tasks) -> UdpRuntime {
    let (session_spawner, retention) = spawn_process_actors(tasks);
    UdpRuntime {
        session_table: None,
        connector: Arc::new(UdpConnector::new(
            connector_config_cell(ConnectorConfig::default()).0,
        )),
        time_validator: Arc::new(TimeValidator::new(
            VALIDATOR_TIME_FRAME + VALIDATOR_UDP_HDR_TTL,
        )),
        session_spawner,
        retention,
    }
}

/// Spawn the production UDP access handler bound to a real socket, routing
/// every datagram to `destination`.
async fn spawn_udp_access(tasks: &mut Tasks, destination: InternetAddr) -> std::net::SocketAddr {
    let handler = UdpAccessConnHandler::new(
        RouteSelector::Empty,
        destination,
        f64::INFINITY,
        udp_context(tasks),
    );
    let server = handler.build("127.0.0.1:0").await.unwrap();
    let addr = server.listener().local_addr().unwrap();
    let (set_conn_handler_tx, set_conn_handler_rx) = loading::replace_conn_handler_channel();
    tasks.spawn(async move {
        let _set_conn_handler_tx = set_conn_handler_tx;
        server.serve(set_conn_handler_rx).await.unwrap();
    });
    addr
}

/// A UDP origin that asserts each datagram equals `req` and answers `resp`.
async fn spawn_udp_greet(
    tasks: &mut Tasks,
    req: &[u8],
    resp: &[u8],
    accepts: usize,
) -> InternetAddr {
    let listener = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let req = req.to_vec();
    let resp = resp.to_vec();
    tasks.spawn(async move {
        for _ in 0..accepts {
            let mut buf = [0; 1024];
            let (len, addr) = listener.recv_from(&mut buf).await.unwrap();
            assert_eq!(&buf[..len], req, "the origin got a corrupted datagram");
            listener.send_to(&resp, addr).await.unwrap();
        }
    });
    addr.into()
}

/// A UDP origin that replies `resp` to `accepts` datagrams of any content.
async fn spawn_udp_responder(tasks: &mut Tasks, resp: &[u8], accepts: usize) -> InternetAddr {
    let listener = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let resp = resp.to_vec();
    tasks.spawn(async move {
        for _ in 0..accepts {
            let mut buf = [0; 1024];
            let (_len, addr) = listener.recv_from(&mut buf).await.unwrap();
            listener.send_to(&resp, addr).await.unwrap();
        }
    });
    addr.into()
}

#[tokio::test(flavor = "multi_thread")]
async fn a_datagram_reaches_the_configured_destination_and_the_reply_returns() {
    let mut tasks = Tasks::new();
    let req_msg = b"ping through the udp access server";
    let resp_msg = b"pong through the udp access server";
    let origin = spawn_udp_greet(&mut tasks, req_msg, resp_msg, 4).await;
    let server_addr = spawn_udp_access(&mut tasks, origin).await;

    let client = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    client.send_to(req_msg, server_addr).await.unwrap();
    let mut buf = [0u8; 2048];
    let (n, _) = tokio::time::timeout(Duration::from_secs(10), client.recv_from(&mut buf))
        .await
        .expect("timed out waiting for the routed reply")
        .unwrap();
    assert_eq!(
        &buf[..n],
        resp_msg,
        "the reply must be relayed byte-exact without a proxy header"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_second_datagram_on_the_same_flow_is_relayed() {
    let mut tasks = Tasks::new();
    let first = b"first datagram";
    let second = b"second datagram";
    let resp_msg = b"reply to both";
    let origin = spawn_udp_responder(&mut tasks, resp_msg, 4).await;
    let server_addr = spawn_udp_access(&mut tasks, origin).await;

    let client = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let mut buf = [0u8; 2048];
    client.send_to(first, server_addr).await.unwrap();
    let (n, _) = tokio::time::timeout(Duration::from_secs(10), client.recv_from(&mut buf))
        .await
        .expect("timed out waiting for the first reply")
        .unwrap();
    assert_eq!(&buf[..n], resp_msg);

    // The same client socket reuses the established flow.
    client.send_to(second, server_addr).await.unwrap();
    let (n, _) = tokio::time::timeout(Duration::from_secs(10), client.recv_from(&mut buf))
        .await
        .expect("timed out waiting for the second reply")
        .unwrap();
    assert_eq!(
        &buf[..n],
        resp_msg,
        "a second datagram on the same flow must still be relayed"
    );
}
