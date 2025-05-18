use std::{
    io,
    net::SocketAddr,
    sync::{Arc, RwLock},
};

use crate::{addr::any_addr, connect::ConnectorConfig};
use tokio::net::UdpSocket;

#[derive(Debug, Clone)]
pub struct UdpConnector {
    config: Arc<RwLock<ConnectorConfig>>,
}
impl UdpConnector {
    pub fn new(config: Arc<RwLock<ConnectorConfig>>) -> Self {
        Self { config }
    }
    pub fn config(&self) -> &Arc<RwLock<ConnectorConfig>> {
        &self.config
    }
    pub async fn connect(&self, addr: SocketAddr) -> io::Result<UdpSocket> {
        let bind = self
            .config
            .read()
            .unwrap()
            .bind
            .get_matched(&addr.ip())
            .map(|ip| SocketAddr::new(ip, 0))
            .unwrap_or_else(|| any_addr(&addr.ip()));
        let socket = UdpSocket::bind(bind).await?;
        socket.connect(addr).await?;
        Ok(socket)
    }
}
