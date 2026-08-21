use std::{net::SocketAddr, sync::Arc};

use tokio::net::{TcpListener, TcpStream};
use tracing::instrument;

use common::{
    error::AnyResult,
    loading,
    session::{SessionSpawner, log_rejection},
    stream::StreamServerHandleConn,
};

use super::proxy_server::AddressedTcpStream;

pub const TCP_STREAM_TYPE: &str = "tcp";

#[derive(Debug)]
pub struct TcpServer<ConnHandler> {
    listener: TcpListener,
    conn_handler: ConnHandler,
    session_spawner: SessionSpawner,
}
impl<ConnHandler> TcpServer<ConnHandler> {
    pub fn new(
        listener: TcpListener,
        conn_handler: ConnHandler,
        session_spawner: SessionSpawner,
    ) -> Self {
        Self {
            listener,
            conn_handler,
            session_spawner,
        }
    }

    pub fn listener(&self) -> &TcpListener {
        &self.listener
    }

    pub fn listener_mut(&mut self) -> &mut TcpListener {
        &mut self.listener
    }
}
impl<ConnHandler> loading::Serve for TcpServer<ConnHandler>
where
    ConnHandler: StreamServerHandleConn + Send + Sync + 'static,
{
    type ConnHandler = ConnHandler;

    async fn serve(
        self,
        set_conn_handler_rx: loading::ReplaceConnHandlerRx<Self::ConnHandler>,
    ) -> AnyResult {
        self.serve_(set_conn_handler_rx).await.map_err(|e| e.into())
    }
}
impl<ConnHandler> TcpServer<ConnHandler>
where
    ConnHandler: StreamServerHandleConn + Send + Sync + 'static,
{
    #[instrument(skip_all)]
    async fn serve_(
        self,
        set_conn_handler_rx: loading::ReplaceConnHandlerRx<ConnHandler>,
    ) -> Result<(), ServeLoopError> {
        let addr = self
            .listener
            .local_addr()
            .map_err(ServeLoopError::LocalAddr)?;
        let listener = &self.listener;
        let session_spawner = self.session_spawner.clone();
        let mut loop_state = ();
        common::lifecycle::serve_loop::serve_loop(
            addr,
            Arc::new(self.conn_handler),
            set_conn_handler_rx,
            |_| {},
            || listener.accept(),
            |_, (stream, _): (TcpStream, SocketAddr), conn_handler: Arc<ConnHandler>| {
                let session_spawner = session_spawner.clone();
                Box::pin(async move {
                    if let Err(error) = session_spawner
                        .spawn(async move {
                            conn_handler.handle_stream(AddressedTcpStream(stream)).await;
                            Ok(())
                        })
                        .await
                    {
                        log_rejection("tcp", error);
                    }
                })
            },
            &mut loop_state,
            |_| Box::pin(std::future::pending::<()>()),
            common::lifecycle::serve_loop::ServeLoopConfig {
                label: "tcp",
                counter_name: Some("stream.tcp.accepts"),
                counts_dispatch_errors: false,
            },
        )
        .await
    }
}
pub use common::lifecycle::serve_loop::ServeLoopError;
