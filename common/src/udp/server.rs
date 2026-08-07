use std::{
    io,
    net::SocketAddr,
    num::NonZeroUsize,
    sync::{Arc, RwLock},
};

use thiserror::Error;
use tokio::net::UdpSocket;
use tokio::sync::{Notify, watch};
use tracing::instrument;
use udp_listener::{Conn, Dispatch, UtpListener};

use crate::{
    error::AnyResult,
    loading,
    proto::conn::udp::{DownstreamAddr, Flow, UpstreamAddr},
    session::{SessionSpawner, log_rejection},
    udp::Packet,
};

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
        let conn_handler = Arc::new(RwLock::new(Arc::new(self.conn_handler)));
        let dispatch = {
            let conn_handler = Arc::clone(&conn_handler);
            move |&addr: &SocketAddr, packet: udp_listener::Packet| -> Option<(Flow, Packet)> {
                let conn_handler = Arc::clone(&conn_handler.read().unwrap());
                let mut buf_reader = io::Cursor::new(&packet[..]);
                let upstream_addr = conn_handler.parse_upstream_addr(&mut buf_reader)?;
                let flow = Flow {
                    upstream: upstream_addr,
                    downstream: DownstreamAddr(addr),
                };
                let read = buf_reader.position() as usize;
                let mut packet = Packet::new(packet);
                packet.advance(read).ok()?;
                Some((flow, packet))
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
        let initial = Arc::clone(&conn_handler.read().unwrap());
        let swap = |new: Arc<ConnHandler>| {
            *conn_handler.write().unwrap() = new;
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
        let serve_result = crate::serve_loop::serve_loop(
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
            |_: &mut (), flow: Conn<UdpSocket, Flow, Packet>, current: Arc<ConnHandler>| {
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
            crate::serve_loop::ServeLoopConfig {
                label: "udp",
                counter_name: None,
                counts_dispatch_errors: false,
            },
        )
        .await;
        // The listener is removed: tell the dispatcher to drain and stop.
        accept_done.notify_one();
        serve_result.map_err(|e| match e {
            crate::serve_loop::ServeLoopError::LocalAddr(e) => UdpServerServeError::LocalAddr(e),
            crate::serve_loop::ServeLoopError::Accept { source, addr } => {
                UdpServerServeError::RecvFrom { source, addr }
            }
        })
    }
}

/// Keep dispatching to a removed listener's surviving flows until the last one
/// closes, then return so the socket can be released. New flows are refused by
/// consuming and dropping the queued connection.
async fn drain_until_idle(
    listener: &UtpListener<UdpSocket, Flow, Packet>,
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
    fn parse_upstream_addr(&self, buf: &mut io::Cursor<&[u8]>) -> Option<Option<UpstreamAddr>>;

    fn handle_flow(&self, conn: Conn<UdpSocket, Flow, Packet>) -> impl Future<Output = ()> + Send;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::loading::Serve;
    use std::time::Duration;
    #[derive(Debug)]
    struct TagEcho(u8);
    impl loading::HandleConn for TagEcho {}
    impl UdpServerHandleConn for TagEcho {
        fn parse_upstream_addr(&self, buf: &mut io::Cursor<&[u8]>) -> Option<Option<UpstreamAddr>> {
            let mut tag = [0; 1];
            std::io::Read::read_exact(buf, &mut tag).ok()?;
            if tag[0] != self.0 {
                return None;
            }
            Some(None)
        }
        async fn handle_flow(&self, conn: Conn<UdpSocket, Flow, Packet>) {
            let (mut read, write) = conn.split();
            while let Some(packet) = read.read_half().recv().await {
                let _ = write.send(packet.slice()).await;
            }
        }
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
