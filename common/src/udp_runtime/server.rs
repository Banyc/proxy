use std::{io, net::SocketAddr, num::NonZeroUsize, sync::Arc};

use thiserror::Error;
use tokio::net::UdpSocket;
use tokio::sync::{Notify, watch};
use tracing::instrument;
use udp_listener::{Classified, Conn, Dispatch, DispatchPolicy, UtpListener};

use crate::{
    error::AnyResult,
    loading,
    proxy_runtime::conn::udp::{DownstreamAddr, Flow, FlowKey, UdpFlowId, UpstreamAddr},
    session::{SessionSpawner, log_rejection},
    udp_runtime::Packet,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UdpPacketRoute {
    Routed {
        flow_id: Option<UdpFlowId>,
        upstream: Option<UpstreamAddr>,
    },
    Compact {
        flow_id: UdpFlowId,
    },
}
fn classify_flow(
    downstream: DownstreamAddr,
    route: UdpPacketRoute,
) -> (FlowKey, Option<Option<UpstreamAddr>>) {
    match route {
        UdpPacketRoute::Routed {
            flow_id: None,
            upstream,
        } => (
            FlowKey::Routed(Flow {
                upstream,
                downstream,
            }),
            None,
        ),
        UdpPacketRoute::Routed {
            flow_id: Some(flow_id),
            upstream,
        } => (
            FlowKey::Identified {
                downstream,
                flow_id,
            },
            Some(upstream),
        ),
        UdpPacketRoute::Compact { flow_id } => (
            FlowKey::Identified {
                downstream,
                flow_id,
            },
            None,
        ),
    }
}

#[derive(Debug)]
pub struct UdpServer<ConnHandler> {
    listener: UdpSocket,
    conn_handler: ConnHandler,
    session_spawner: SessionSpawner,
}
impl<ConnHandler> UdpServer<ConnHandler> {
    pub fn new(
        listener: UdpSocket,
        conn_handler: ConnHandler,
        session_spawner: SessionSpawner,
    ) -> Self {
        Self {
            listener,
            conn_handler,
            session_spawner,
        }
    }

    pub fn listener(&self) -> &UdpSocket {
        &self.listener
    }

    pub fn listener_mut(&mut self) -> &mut UdpSocket {
        &mut self.listener
    }
}
impl<ConnHandler> loading::Serve for UdpServer<ConnHandler>
where
    ConnHandler: UdpServerHandleConn + Send + Sync + 'static,
{
    type ConnHandler = ConnHandler;

    async fn serve(
        self,
        set_conn_handler_rx: loading::ReplaceConnHandlerRx<Self::ConnHandler>,
    ) -> AnyResult {
        self.serve_(set_conn_handler_rx).await.map_err(|e| e.into())
    }
}
impl<ConnHandler> UdpServer<ConnHandler>
where
    ConnHandler: UdpServerHandleConn + Send + Sync + 'static,
{
    #[instrument(skip_all)]
    async fn serve_(
        self,
        set_conn_handler_rx: loading::ReplaceConnHandlerRx<ConnHandler>,
    ) -> Result<(), UdpServerServeError> {
        let reloadable = loading::ReloadableHandler::new(self.conn_handler);
        let dispatch = {
            let reloadable = reloadable.clone();
            move |&addr: &SocketAddr,
                  packet: udp_listener::Packet|
                  -> Option<Classified<FlowKey, Packet>> {
                let conn_handler = reloadable.current();
                let mut buf_reader = io::Cursor::new(&packet[..]);
                let route = conn_handler.parse_packet_route(&mut buf_reader)?;
                // A compact datagram can only continue a live flow: when its
                // conntrack key is absent the listener must drop it before
                // allocating any channel, conntrack entry, or handler task.
                let policy = if matches!(route, UdpPacketRoute::Compact { .. }) {
                    DispatchPolicy::ExistingOnly
                } else {
                    DispatchPolicy::Create
                };
                let downstream = DownstreamAddr(addr);
                let (flow_key, routed_upstream) = classify_flow(downstream, route);
                let read = buf_reader.position() as usize;
                let mut packet = Packet::new(packet);
                packet.advance(read).ok()?;
                if let Some(upstream) = routed_upstream {
                    packet.set_routed_upstream(upstream);
                }
                Some(Classified {
                    key: flow_key,
                    value: packet,
                    policy,
                })
            }
        };
        let addr = self
            .listener
            .local_addr()
            .map_err(UdpServerServeError::LocalAddr)?;
        let dispatcher_buffer_size = NonZeroUsize::new(64).unwrap();
        let downstream_listener = Arc::new(UtpListener::new(
            self.listener,
            dispatcher_buffer_size,
            Arc::new(dispatch),
        ));
        let session_spawner = self.session_spawner;
        let initial = reloadable.current();
        let swap = {
            let reloadable = reloadable.clone();
            move |new: Arc<ConnHandler>| {
                reloadable.replace(new);
            }
        };

        // Packet dispatch is process-scoped while flow handlers are
        // process-scoped too: a removed listener must keep routing datagrams
        // to its surviving flows, so dispatch runs here (in the session scope)
        // rather than inside the listener's accept loop.
        let dispatcher_idle = downstream_listener.idle();
        let accept_done = Arc::new(Notify::new());
        let dispatcher_done = Arc::new(Notify::new());
        {
            let dispatcher_listener = Arc::clone(&downstream_listener);
            let accept_done = Arc::clone(&accept_done);
            let dispatcher_done = Arc::clone(&dispatcher_done);
            let mut dispatcher_idle = dispatcher_idle;
            if let Err(error) = session_spawner
                .spawn(async move {
                    loop {
                        tokio::select! {
                            _ = accept_done.notified() => {
                                // The listener is gone: survive on dispatch until
                                // the last flow closes, then stop and release the
                                // socket.
                                drain_until_idle(&dispatcher_listener, &mut dispatcher_idle).await;
                                break;
                            }
                            result = dispatcher_listener.dispatch_next() => {
                                if result.is_err() {
                                    break;
                                }
                            }
                        }
                    }
                    dispatcher_done.notify_one();
                    Ok(())
                })
                .await
            {
                log_rejection("udp_dispatch", error);
            }
        }

        let mut state = ();
        let serve_result = crate::lifecycle::serve_loop::serve_loop(
            addr,
            initial,
            set_conn_handler_rx,
            swap,
            || {
                let listener = Arc::clone(&downstream_listener);
                let dispatcher_done = Arc::clone(&dispatcher_done);
                async move {
                    tokio::select! {
                        conn = listener.accept_next() => conn.ok_or_else(|| {
                            io::Error::new(
                                io::ErrorKind::BrokenPipe,
                                "udp listener accept queue closed",
                            )
                        }),
                        _ = dispatcher_done.notified() => Err(io::Error::new(
                            io::ErrorKind::BrokenPipe,
                            "udp packet dispatcher stopped",
                        )),
                    }
                }
            },
            |_: &mut (), flow: Conn<UdpSocket, FlowKey, Packet>, current: Arc<ConnHandler>| {
                let session_spawner = session_spawner.clone();
                Box::pin(async move {
                    if let Err(error) = session_spawner
                        .spawn(async move {
                            current.handle_flow(flow).await;
                            Ok(())
                        })
                        .await
                    {
                        log_rejection("udp_flow", error);
                    }
                })
            },
            &mut state,
            |_| Box::pin(std::future::pending::<()>()),
            crate::lifecycle::serve_loop::ServeLoopConfig {
                label: "udp",
                counter_name: None,
                counts_dispatch_errors: false,
            },
        )
        .await;
        // The listener is removed: tell the dispatcher to drain and stop.
        accept_done.notify_one();
        serve_result.map_err(|e| match e {
            crate::lifecycle::serve_loop::ServeLoopError::LocalAddr(e) => {
                UdpServerServeError::LocalAddr(e)
            }
            crate::lifecycle::serve_loop::ServeLoopError::Accept { source, addr } => {
                UdpServerServeError::RecvFrom { source, addr }
            }
        })
    }
}

/// Keep dispatching to a removed listener's surviving flows until the last one
/// closes, then return so the socket can be released. New flows are refused by
/// consuming and dropping the queued connection.
async fn drain_until_idle(
    listener: &UtpListener<UdpSocket, FlowKey, Packet>,
    idle_rx: &mut watch::Receiver<bool>,
) {
    // Drop any queued-but-unaccepted flows so their connection entries close.
    while let Some(conn) = listener.try_accept_next() {
        drop(conn);
    }
    loop {
        if *idle_rx.borrow_and_update() {
            return;
        }
        tokio::select! {
            _ = idle_rx.changed() => {
                if *idle_rx.borrow_and_update() {
                    return;
                }
            }
            result = listener.dispatch_next() => match result {
                Err(_) => return,
                Ok(Dispatch::Accepted) => {
                    let _ = listener.accept_next().await;
                }
                Ok(Dispatch::Routed) => {}
            },
        }
    }
}
#[derive(Debug, Error)]
pub enum UdpServerServeError {
    #[error("Failed to get local address: {0}")]
    LocalAddr(#[source] io::Error),
    #[error("Failed to receive packet from downstream: {source}, {addr}")]
    RecvFrom {
        #[source]
        source: io::Error,
        addr: SocketAddr,
    },
}

pub trait UdpServerHandleConn: loading::HandleConn {
    fn parse_packet_route(&self, buf: &mut io::Cursor<&[u8]>) -> Option<UdpPacketRoute>;

    fn handle_flow(
        &self,
        conn: Conn<UdpSocket, FlowKey, Packet>,
    ) -> impl Future<Output = ()> + Send;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proxy_runtime::conn::udp::UDP_FLOW_ID_LEN;
    use crate::{loading::Serve, proxy_runtime::addr::RouteAddr};
    use std::time::Duration;
    #[derive(Debug)]
    struct TagEcho(u8);
    impl loading::HandleConn for TagEcho {}
    impl UdpServerHandleConn for TagEcho {
        fn parse_packet_route(&self, buf: &mut io::Cursor<&[u8]>) -> Option<UdpPacketRoute> {
            let mut tag = [0; 1];
            std::io::Read::read_exact(buf, &mut tag).ok()?;
            if tag[0] != self.0 {
                return None;
            }
            Some(UdpPacketRoute::Routed {
                flow_id: None,
                upstream: None,
            })
        }
        async fn handle_flow(&self, conn: Conn<UdpSocket, FlowKey, Packet>) {
            let (mut read, write) = conn.split();
            while let Some(packet) = read.read_half().recv().await {
                let _ = write.send(packet.slice()).await;
            }
        }
    }

    #[test]
    fn routed_and_compact_proxy_packets_share_a_conntrack_key() {
        let downstream = DownstreamAddr("127.0.0.1:4000".parse().unwrap());
        let flow_id = UdpFlowId::from_bytes([3; UDP_FLOW_ID_LEN]);
        let upstream = Some(UpstreamAddr(RouteAddr::udp(
            "127.0.0.1:9".parse::<SocketAddr>().unwrap().into(),
        )));
        let (routed_key, routed_metadata) = classify_flow(
            downstream,
            UdpPacketRoute::Routed {
                flow_id: Some(flow_id),
                upstream: upstream.clone(),
            },
        );
        let (compact_key, compact_metadata) =
            classify_flow(downstream, UdpPacketRoute::Compact { flow_id });
        assert_eq!(routed_key, compact_key);
        assert!(matches!(routed_key, FlowKey::Identified { .. }));
        assert_eq!(routed_metadata, Some(upstream));
        assert_eq!(compact_metadata, None);
    }

    #[test]
    fn route_keyed_flows_keep_the_upstream_in_their_identity() {
        let downstream = DownstreamAddr("127.0.0.1:4000".parse().unwrap());
        let a = Some(UpstreamAddr(RouteAddr::udp(
            "127.0.0.1:9".parse::<SocketAddr>().unwrap().into(),
        )));
        let b = Some(UpstreamAddr(RouteAddr::udp(
            "127.0.0.1:10".parse::<SocketAddr>().unwrap().into(),
        )));
        let (key_a, metadata_a) = classify_flow(
            downstream,
            UdpPacketRoute::Routed {
                flow_id: None,
                upstream: a,
            },
        );
        let (key_b, metadata_b) = classify_flow(
            downstream,
            UdpPacketRoute::Routed {
                flow_id: None,
                upstream: b,
            },
        );
        assert_ne!(key_a, key_b);
        assert_eq!(metadata_a, None);
        assert_eq!(metadata_b, None);
    }

    #[tokio::test]
    async fn a_hot_reload_reaches_the_dispatcher() {
        let listener = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (set_conn_handler_tx, set_conn_handler_rx) = loading::replace_conn_handler_channel();
        let (session_spawner, mut session_rx) = crate::session::SessionSpawner::channel();
        let mut session_tasks = tokio::task::JoinSet::new();
        session_tasks.spawn(async move {
            let mut sessions = tokio::task::JoinSet::new();
            loop {
                tokio::select! {
                    Some(fut) = session_rx.recv() => { sessions.spawn(fut); }
                    Some(res) = sessions.join_next() => { let _ = res.unwrap(); }
                    else => break,
                }
            }
        });
        let mut server_tasks = tokio::task::JoinSet::new();
        server_tasks.spawn(
            UdpServer::new(listener, TagEcho(1), session_spawner).serve(set_conn_handler_rx),
        );
        let echoed = |tag: u8| async move {
            let client = UdpSocket::bind("127.0.0.1:0").await.unwrap();
            for _ in 0..50 {
                client.send_to(&[tag, 42], addr).await.unwrap();
                let mut buf = [0; 8];
                let recv = tokio::time::timeout(Duration::from_millis(100), client.recv(&mut buf));
                if let Ok(Ok(n)) = recv.await {
                    return Some(buf[..n].to_vec());
                }
            }
            None
        };
        assert_eq!(echoed(1).await.as_deref(), Some(&[42][..]));
        set_conn_handler_tx.send(TagEcho(2)).unwrap();
        assert_eq!(echoed(2).await.as_deref(), Some(&[42][..]));
    }

    #[tokio::test]
    async fn removing_the_listener_keeps_surviving_flows_dispatching() {
        let listener = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (set_conn_handler_tx, set_conn_handler_rx) = loading::replace_conn_handler_channel();
        let (session_spawner, mut session_rx) = crate::session::SessionSpawner::channel();
        let mut session_tasks = tokio::task::JoinSet::new();
        session_tasks.spawn(async move {
            let mut sessions = tokio::task::JoinSet::new();
            loop {
                tokio::select! {
                    Some(fut) = session_rx.recv() => { sessions.spawn(fut); }
                    Some(res) = sessions.join_next() => { let _ = res.unwrap(); }
                    else => break,
                }
            }
        });
        let mut server_tasks = tokio::task::JoinSet::new();
        server_tasks.spawn(
            UdpServer::new(listener, TagEcho(1), session_spawner).serve(set_conn_handler_rx),
        );
        let client = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let echoed = async {
            for _ in 0..50 {
                client.send_to(&[1, 42], addr).await.unwrap();
                let mut buf = [0; 8];
                let recv = tokio::time::timeout(Duration::from_millis(100), client.recv(&mut buf));
                if let Ok(Ok(n)) = recv.await {
                    return Some(buf[..n].to_vec());
                }
            }
            None
        };
        assert_eq!(echoed.await.as_deref(), Some(&[42][..]));
        // Remove the listener: the serve task must despawn...
        drop(set_conn_handler_tx);
        tokio::time::timeout(Duration::from_secs(5), server_tasks.join_next())
            .await
            .expect("the listener never despawned after removal")
            .unwrap()
            .unwrap()
            .unwrap();
        // ...while the already-accepted flow keeps receiving packet dispatch.
        let echoed_again = async {
            for _ in 0..50 {
                client.send_to(&[1, 42], addr).await.unwrap();
                let mut buf = [0; 8];
                let recv = tokio::time::timeout(Duration::from_millis(100), client.recv(&mut buf));
                if let Ok(Ok(n)) = recv.await {
                    return Some(buf[..n].to_vec());
                }
            }
            None
        };
        assert_eq!(
            echoed_again.await.as_deref(),
            Some(&[42][..]),
            "removing the listener left the surviving flow without packet dispatch"
        );
    }
}
