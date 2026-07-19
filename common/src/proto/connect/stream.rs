use std::{
    collections::HashMap,
    io,
    net::SocketAddr,
    ops::Deref,
    sync::{Arc, RwLock},
    time::Duration,
};

use async_trait::async_trait;

use crate::{connect::ConnectorConfig, stream::AsConn};

#[async_trait]
pub trait StreamConnect: std::fmt::Debug + Sync + Send + 'static {
    async fn connect(&self, addr: SocketAddr) -> io::Result<Box<dyn AsConn>>;
}
pub trait StreamConnectExt: StreamConnect {
    fn timed_connect(
        &self,
        addr: SocketAddr,
        timeout: Duration,
    ) -> impl Future<Output = io::Result<Box<dyn AsConn>>> + Send
    where
        Self: Sync,
    {
        async move {
            let res = tokio::time::timeout(timeout, self.connect(addr)).await;
            match res {
                Ok(res) => res,
                Err(_) => Err(io::Error::new(io::ErrorKind::TimedOut, "Timed out")),
            }
        }
    }
}
impl<T: StreamConnect + ?Sized> StreamConnectExt for T {}

#[derive(Debug)]
pub struct StreamConnectorTable {
    config: Arc<RwLock<ConnectorConfig>>,
    connectors: HashMap<Arc<str>, Arc<dyn StreamConnect>>,
}
impl StreamConnectorTable {
    pub fn new(
        config: Arc<RwLock<ConnectorConfig>>,
        connectors: HashMap<Arc<str>, Arc<dyn StreamConnect>>,
    ) -> Self {
        Self { config, connectors }
    }
    pub fn replaced_by(&self, config: ConnectorConfig) {
        *self.config.write().unwrap() = config;
    }
}
impl StreamConnectorTable {
    pub async fn timed_connect_2(
        &self,
        stream_type: &str,
        addrs: impl IntoIterator<Item = SocketAddr>,
        timeout: Duration,
    ) -> io::Result<(Box<dyn AsConn>, SocketAddr)> {
        let mut last_res = None;
        for addr in addrs {
            let res = self.timed_connect(stream_type, addr, timeout).await;
            match res {
                Ok(res) => {
                    last_res = Some(Ok((res, addr)));
                    break;
                }
                Err(e) => last_res = Some(Err(e)),
            }
        }
        last_res.unwrap_or_else(|| Err(io::Error::other("no addrs")))
    }

    pub async fn timed_connect(
        &self,
        stream_type: &str,
        addr: SocketAddr,
        timeout: Duration,
    ) -> io::Result<Box<dyn AsConn>> {
        let Some(connector) = self.connectors.get(stream_type) else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("invalid stream type: `{stream_type}`"),
            ));
        };
        StreamConnectExt::timed_connect(connector.deref(), addr, timeout).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::stream::{HasIoAddr, OwnIoStream};
    use std::pin::Pin;
    use std::task::{Context, Poll};
    use tokio::io::{AsyncRead, AsyncWrite, DuplexStream, ReadBuf};

    const STREAM_TYPE: &str = "test";

    #[derive(Debug)]
    struct TestConn {
        io: DuplexStream,
        addr: SocketAddr,
    }

    impl AsyncRead for TestConn {
        fn poll_read(
            mut self: Pin<&mut Self>,
            cx: &mut Context<'_>,
            buf: &mut ReadBuf<'_>,
        ) -> Poll<io::Result<()>> {
            Pin::new(&mut self.io).poll_read(cx, buf)
        }
    }

    impl AsyncWrite for TestConn {
        fn poll_write(
            mut self: Pin<&mut Self>,
            cx: &mut Context<'_>,
            buf: &[u8],
        ) -> Poll<io::Result<usize>> {
            Pin::new(&mut self.io).poll_write(cx, buf)
        }
        fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            Pin::new(&mut self.io).poll_flush(cx)
        }
        fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            Pin::new(&mut self.io).poll_shutdown(cx)
        }
    }

    impl HasIoAddr for TestConn {
        fn peer_addr(&self) -> io::Result<SocketAddr> {
            Ok(self.addr)
        }
        fn local_addr(&self) -> io::Result<SocketAddr> {
            Ok(self.addr)
        }
    }

    impl OwnIoStream for TestConn {}
    impl AsConn for TestConn {}

    #[derive(Debug)]
    struct FallbackConnector {
        attempted: std::sync::Mutex<Vec<SocketAddr>>,
        successful: SocketAddr,
    }

    #[async_trait]
    impl StreamConnect for FallbackConnector {
        async fn connect(&self, addr: SocketAddr) -> io::Result<Box<dyn AsConn>> {
            self.attempted.lock().unwrap().push(addr);
            if addr != self.successful {
                return Err(io::Error::from(io::ErrorKind::NetworkUnreachable));
            }
            let (io, _peer) = tokio::io::duplex(1);
            Ok(Box::new(TestConn { io, addr }))
        }
    }

    #[tokio::test]
    async fn tries_next_resolved_address_after_non_refused_error() {
        let unreachable = "[2001:db8::1]:443".parse().unwrap();
        let successful = "192.0.2.1:443".parse().unwrap();
        let connector = Arc::new(FallbackConnector {
            attempted: std::sync::Mutex::new(Vec::new()),
            successful,
        });
        let table = StreamConnectorTable::new(
            Arc::new(RwLock::new(ConnectorConfig::default())),
            HashMap::from([(
                Arc::from(STREAM_TYPE),
                connector.clone() as Arc<dyn StreamConnect>,
            )]),
        );
        let (_, connected_addr) = table
            .timed_connect_2(
                STREAM_TYPE,
                [unreachable, successful],
                Duration::from_secs(1),
            )
            .await
            .unwrap();
        assert_eq!(connected_addr, successful);
        assert_eq!(
            *connector.attempted.lock().unwrap(),
            [unreachable, successful]
        );
    }
}
