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
