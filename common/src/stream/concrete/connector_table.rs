use std::{io, net::SocketAddr, sync::Arc, time::Duration};

use once_cell::sync::Lazy;

use super::{
    addr::ConcreteStreamType,
    connect::{StreamConnect, StreamConnectExt},
    created_stream::CreatedStream,
    streams::{kcp::KcpConnector, mptcp::MptcpConnector, tcp::TcpConnector},
};

pub static STREAM_CONNECTOR_TABLE: Lazy<StreamConnectorTable> =
    Lazy::new(StreamConnectorTable::new);

#[derive(Debug)]
pub struct StreamConnectorTable {
    tcp: Arc<TcpConnector>,
    kcp: Arc<KcpConnector>,
    mptcp: Arc<MptcpConnector>,
}

impl StreamConnectorTable {
    fn new() -> Self {
        Self {
            tcp: Arc::new(TcpConnector),
            kcp: Arc::new(KcpConnector),
            mptcp: Arc::new(MptcpConnector),
        }
    }

    pub async fn connect(
        &self,
        stream_type: ConcreteStreamType,
        addr: SocketAddr,
    ) -> io::Result<CreatedStream> {
        match stream_type {
            ConcreteStreamType::Tcp => self.tcp.connect(addr).await,
            ConcreteStreamType::Kcp => self.kcp.connect(addr).await,
            ConcreteStreamType::Mptcp => self.mptcp.connect(addr).await,
        }
    }

    pub async fn timed_connect(
        &self,
        stream_type: ConcreteStreamType,
        addr: SocketAddr,
        timeout: Duration,
    ) -> io::Result<CreatedStream> {
        match stream_type {
            ConcreteStreamType::Tcp => self.tcp.timed_connect(addr, timeout).await,
            ConcreteStreamType::Kcp => self.kcp.timed_connect(addr, timeout).await,
            ConcreteStreamType::Mptcp => self.mptcp.timed_connect(addr, timeout).await,
        }
    }
}

impl Default for StreamConnectorTable {
    fn default() -> Self {
        Self::new()
    }
}
