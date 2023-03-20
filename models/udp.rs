use std::{io, net::SocketAddr, ops::Deref, sync::Arc};

use tokio::net::UdpSocket;

#[derive(Debug, Clone)]
pub struct UdpDownstreamWriter {
    downstream_writer: Arc<UdpSocket>,
    downstream_addr: SocketAddr,
}

impl UdpDownstreamWriter {
    pub fn new(downstream_writer: Arc<UdpSocket>, downstream_addr: SocketAddr) -> Self {
        Self {
            downstream_writer,
            downstream_addr,
        }
    }

    pub async fn send(&self, buf: &[u8]) -> io::Result<usize> {
        self.downstream_writer
            .send_to(buf, self.downstream_addr)
            .await
    }

    pub fn remote_addr(&self) -> SocketAddr {
        self.downstream_addr
    }
}

impl Deref for UdpDownstreamWriter {
    type Target = UdpSocket;

    fn deref(&self) -> &Self::Target {
        &self.downstream_writer
    }
}
